//! The `#[global_allocator]` front end.
//!
//! [`Bcmalloc`] is a `static` that creates its [`Region`] the first time anything allocates, and
//! gives every thread a [`Heap`] of its own. After that a call looks like this.
//!
//! ```text
//!    alloc(layout)
//!         |
//!         v
//!    read this thread's heap from its slot (see the tls module)
//!         |
//!         +-- found ---> table lookup, then pop the hot list ... the fast path, no atomics
//!         |
//!         +-- empty ---> reserve the region if this is the first call, claim a heap,
//!                        bind it to the thread, and allocate from it
//! ```
//!
//! # Thread start-up and teardown
//!
//! A thread binds its heap on its first allocation or free, and a key destructor releases the
//! heap when the thread exits. Other destructors may still allocate or free after that one has
//! run. At that point the slot is empty and the thread is marked as gone, so frees fall back to
//! [`Region`::dealloc] and allocations borrow a heap for just that one call.
//!
//! # Failure
//!
//! A failed allocation returns null, and the standard library turns that into an abort. That
//! happens when the region is full, when the alignment is above [`MAX_ALIGN`](crate::MAX_ALIGN),
//! or when more threads are running than the configuration has heaps for.

use crate::{
    allocator::{Backend, allocator},
    config::Config,
    heap::{Heap, HeapData},
    region::{Error, Mapping, Region, Stats},
    size_class::{MIN_ALIGN, TABLE_MAX},
    sync::{AtomicPtr, AtomicUsize, Ordering},
    tls,
};
use core::{
    alloc::{AllocError, Allocator, GlobalAlloc, Layout},
    ptr::{self, NonNull},
};

/// The smallest region the lazy start-up settles for when the OS refuses a bigger one.
const MIN_CAPACITY: usize = 256 << 20;

/// A global allocator that reserves its memory up front.
///
/// Declare one as the program's allocator and everything else follows.
///
/// ```
/// use bcmalloc::Bcmalloc;
///
/// #[global_allocator]
/// static GLOBAL: Bcmalloc = Bcmalloc::new();
///
/// let numbers: Vec<u64> = (0..1000).collect();
/// assert_eq!(numbers.iter().sum::<u64>(), 499_500);
/// ```
///
/// The region is reserved on first use with the default [`Config`]. Use [`Bcmalloc::with_config`]
/// to size it for the program. If the operating system refuses the reservation, the allocator
/// retries with half the capacity, down to 256 MiB.
///
/// ```
/// use bcmalloc::{Bcmalloc, Commit, Config};
///
/// #[global_allocator]
/// static GLOBAL: Bcmalloc =
///     Bcmalloc::with_config(Config::new().capacity(8 << 30).max_heaps(256).commit(Commit::Lazy));
///
/// let text = String::from("allocated from an 8 GiB region");
/// assert_eq!(GLOBAL.stats().unwrap().heaps_in_use, 1);
/// # drop(text);
/// ```
///
/// # As an `Allocator`
///
/// A `Bcmalloc` can also serve a single collection through the standard library's
/// [`Allocator`] trait, whether or not it is the global allocator.
///
/// ```
/// use bcmalloc::Bcmalloc;
///
/// static ALLOCATOR: Bcmalloc = Bcmalloc::new();
///
/// let mut numbers = Vec::new_in(&ALLOCATOR);
/// numbers.extend(0..1000_u32);
/// assert_eq!(numbers.iter().sum::<u32>(), 499_500);
///
/// let boxed = Box::new_in([7_u8; 100], &ALLOCATOR);
/// assert_eq!(boxed[99], 7);
/// ```
///
/// A zero-sized request never reaches the allocator. A block is exactly as long as it was asked to
/// be, zeroed requests clear all of it, and a resize that changes the alignment moves the block.
#[derive(Debug)]
pub struct Bcmalloc {
    config: Config,
    region: AtomicPtr<Region<'static>>,
    /// The thread-specific key under which threads keep their heap, or `tls::NO_KEY`.
    key: AtomicUsize,
}

impl Bcmalloc {
    /// An allocator with the default [`Config`].
    pub const fn new() -> Self {
        Self::with_config(Config::new())
    }

    /// An allocator with the given configuration.
    pub const fn with_config(config: Config) -> Self {
        Self { config, region: AtomicPtr::new(ptr::null_mut()), key: AtomicUsize::new(tls::NO_KEY) }
    }

    /// The configuration the region was or will be created with.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// A snapshot of the region, or `None` if nothing has been allocated yet.
    pub fn stats(&self) -> Option<Stats> {
        self.existing().map(Region::stats)
    }

    /// A snapshot of the allocator's metrics, or `None` if nothing has been allocated yet. See
    /// [`Region::metrics`].
    #[cfg(feature = "metrics")]
    pub fn metrics(&self) -> Option<crate::Metrics> {
        self.existing().map(Region::metrics)
    }

    /// Returns the memory of idle heaps and free chunks to the operating system. See
    /// [`Region::trim`] for what that costs.
    pub fn trim(&self) {
        if let Some(region) = self.existing() {
            region.trim();
        }
    }

    /// The layout of the whole block that `ptr` points to: its full usable size, and an alignment
    /// of
    /// 16. It is None if `ptr` is not in this allocator's region.
    ///
    /// The answer is only meaningful when `ptr` is the start of a live block from this allocator.
    /// It is what a C interface needs to implement `free` and `malloc_usable_size`, which get no
    /// size, and the layout it returns is a valid one to free or reallocate the block with.
    pub fn block_layout(&self, ptr: NonNull<u8>) -> Option<Layout> {
        self.existing()?.block_layout(ptr.as_ptr())
    }

    /// Allocates `size` bytes aligned to 16, as C's `malloc` does, or returns null if there is no
    /// room. A size of zero gets a block of its own.
    #[inline]
    pub fn malloc(&self, size: usize) -> *mut u8 {
        if size <= TABLE_MAX &&
            let Some(data) = self.thread_heap()
        {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and the table covers
            // the size.
            return unsafe { data.alloc_table(size, true) };
        }
        self.malloc_general(size)
    }

    /// Allocates what [`malloc`](Self::malloc) does not take on its fast path.
    #[inline(never)]
    fn malloc_general(&self, size: usize) -> *mut u8 {
        Layout::from_size_align(size.max(1), MIN_ALIGN).map_or(ptr::null_mut(), |layout| {
            // SAFETY: the layout has a non-zero size.
            unsafe { GlobalAlloc::alloc(self, layout) }
        })
    }

    /// How many bytes the block that `ptr` points to holds, as C's `malloc_usable_size` tells, or
    /// `None` if `ptr` is not in this allocator's region. The answer is only meaningful when `ptr`
    /// is the start of a live block from this allocator.
    #[inline]
    pub fn usable_size(&self, ptr: NonNull<u8>) -> Option<usize> {
        self.thread_heap().map_or_else(
            || self.block_layout(ptr).map(|layout| layout.size()),
            |data| data.usable_size(ptr),
        )
    }

    /// Resizes the block that `ptr` points to without its layout, as C's `realloc` does, keeping
    /// an alignment of 16. Returns null, and leaves the block as it was, if there is no room or
    /// the size is too large, or if `ptr` is not in this allocator's region.
    ///
    /// # Safety
    ///
    /// `ptr` must be the start of a live block from this allocator, and `new_size` must not be
    /// zero.
    pub unsafe fn realloc_unsized(&self, ptr: NonNull<u8>, new_size: usize) -> *mut u8 {
        let Some(size) = self.usable_size(ptr) else { return ptr::null_mut() };
        if Layout::from_size_align(new_size, MIN_ALIGN).is_err() {
            return ptr::null_mut();
        }
        // SAFETY: the block holds `size` bytes at an alignment of at least 16, which is a layout it
        // can be freed and resized with, and the new size is valid for that alignment.
        unsafe {
            let old = Layout::from_size_align_unchecked(size, MIN_ALIGN);
            GlobalAlloc::realloc(self, ptr.as_ptr(), old, new_size)
        }
    }

    /// Frees the block that `ptr` points to without its layout, as C's `free` does, and ignores
    /// a pointer outside the region.
    ///
    /// # Safety
    ///
    /// A pointer inside the region must be the start of a live block from this allocator, and the
    /// block must not be used again.
    #[inline]
    pub unsafe fn free_unsized(&self, ptr: NonNull<u8>) {
        match self.thread_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone, its region is this
            // allocator's, and the caller vouches for the block.
            Some(data) => unsafe { data.free_unsized(ptr) },
            // SAFETY: the caller vouches for the block.
            None => unsafe { self.free_unsized_unbound(ptr) },
        }
    }

    /// Frees without a layout for a thread that has no heap yet.
    #[cold]
    #[inline(never)]
    unsafe fn free_unsized_unbound(&self, ptr: NonNull<u8>) {
        if let Some(layout) = self.block_layout(ptr) {
            // SAFETY: the layout is the one the allocator holds for the block.
            unsafe { self.dealloc_unbound(ptr, layout) };
        }
    }

    /// Hands back the blocks that the calling thread's heap keeps for reuse: its bins, its
    /// outboxes and what other threads sent it, and then its empty slabs.
    ///
    /// A thread that is about to go quiet for a while calls this so that its cached memory is not
    /// held while it sleeps, as jemalloc offers with its thread cache flush. Heaps of exited
    /// threads are handed back on their own, and [`trim`](Self::trim) covers heaps nobody holds.
    pub fn flush_thread(&self) {
        if let (Some(data), Some(region)) = (self.bound_heap(), self.existing()) {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and so is its region.
            unsafe { data.flush(region) };
        }
    }

    /// The region if it exists.
    fn existing(&self) -> Option<&'static Region<'static>> {
        // SAFETY: a non-null pointer is a leaked mapping, which lives for the rest of the program.
        unsafe { self.region.load(Ordering::Acquire).as_ref() }
    }

    /// The region, creating it on first use.
    #[inline]
    fn region(&self) -> Option<&'static Region<'static>> {
        self.existing().or_else(|| self.create())
    }

    #[cold]
    fn create(&self) -> Option<&'static Region<'static>> {
        let mut config = self.config;
        let mapping = loop {
            match Mapping::new(&config) {
                Ok(mapping) => break mapping,
                Err(Error::OutOfAddressSpace) if config.capacity_bytes() / 2 >= MIN_CAPACITY => {
                    config = config.capacity(config.capacity_bytes() / 2);
                }
                Err(_) => return None,
            }
        };
        let candidate = ptr::from_ref(mapping.region()).cast::<Region<'static>>().cast_mut();
        match self.region.compare_exchange(
            ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(mapping.leak()),
            // Another thread got there first. Our mapping has no users, so dropping it unmaps it.
            Err(existing) => {
                drop(mapping);
                // SAFETY: the winner leaked its mapping.
                unsafe { existing.as_ref() }
            }
        }
    }
}

impl Backend for Bcmalloc {
    unsafe fn allocate_raw(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded.
        unsafe { GlobalAlloc::alloc(self, layout) }
    }

    unsafe fn allocate_zeroed_raw(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded.
        unsafe { GlobalAlloc::alloc_zeroed(self, layout) }
    }

    unsafe fn deallocate_raw(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded.
        unsafe { GlobalAlloc::dealloc(self, ptr.as_ptr(), layout) };
    }

    unsafe fn resize_raw(&self, ptr: NonNull<u8>, old: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded.
        unsafe { GlobalAlloc::realloc(self, ptr.as_ptr(), old, new_size) }
    }
}

allocator!(Allocator for Bcmalloc);

impl Default for Bcmalloc {
    fn default() -> Self {
        Self::new()
    }
}

impl Bcmalloc {
    /// This thread's heap, or `None` if it has not bound one or the fast lookup cannot tell.
    #[inline(always)]
    fn thread_heap(&self) -> Option<&'static HeapData> {
        tls::get(self.key.load(Ordering::Relaxed))
    }

    /// This thread's heap, or `None` if it has not bound one. This is the slow lookup that settles
    /// what [`thread_heap`](Self::thread_heap) could not.
    fn bound_heap(&self) -> Option<&'static HeapData> {
        tls::lookup(self.key.load(Ordering::Relaxed))
    }

    /// The key under which threads bind their heap, created on first use.
    fn key(&self) -> Option<usize> {
        let key = self.key.load(Ordering::Acquire);
        if key != tls::NO_KEY {
            return Some(key);
        }
        let created = tls::create_key()?;
        match self.key.compare_exchange(tls::NO_KEY, created, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Some(created),
            // Another thread created the key first. Ours has no users.
            Err(existing) => {
                tls::delete_key(created);
                Some(existing)
            }
        }
    }

    /// Claims a heap for this thread and binds it, or returns None if the thread is exiting, no
    /// heap is free or the system has no key to give.
    #[cold]
    fn bind(&self, region: &'static Region<'static>) -> Option<&'static HeapData> {
        if tls::exiting() || tls::binding() {
            return None;
        }
        tls::set_binding(true);
        let bound = self.bind_now(region);
        tls::set_binding(false);
        bound
    }

    /// The body of [`bind`](Self::bind), which runs with the binding flag set.
    fn bind_now(&self, region: &'static Region<'static>) -> Option<&'static HeapData> {
        let key = self.key()?;
        let heap = region.heap()?;
        // The heap releases its claim if the system refuses to remember it.
        let data = heap.leak();
        if tls::bind(key, data) {
            Some(data)
        } else {
            // SAFETY: the claim was taken just above and nothing else uses the heap.
            unsafe { data.give_up() };
            None
        }
    }

    /// Allocates for a thread that has no heap yet.
    #[cold]
    #[inline(never)]
    unsafe fn alloc_unbound(&self, layout: Layout) -> *mut u8 {
        if let Some(data) = self.bound_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone.
            return unsafe { data.alloc(layout) };
        }
        let Some(region) = self.region() else { return ptr::null_mut() };
        if let Some(data) = self.bind(region) {
            // SAFETY: binding claimed the heap for this thread.
            return unsafe { data.alloc(layout) };
        }
        region.heap().and_then(|heap| heap.alloc(layout)).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    /// Frees for a thread that has no heap yet.
    #[cold]
    #[inline(never)]
    unsafe fn dealloc_unbound(&self, ptr: NonNull<u8>, layout: Layout) {
        if let Some(data) = self.bound_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and the caller vouches
            // for the block.
            return unsafe { data.dealloc(ptr, layout) };
        }
        let Some(region) = self.existing() else { return };
        if let Some(data) = self.bind(region) {
            // SAFETY: binding claimed the heap for this thread, and the caller vouches for the
            // block.
            unsafe { data.dealloc(ptr, layout) };
        } else {
            // SAFETY: as above.
            unsafe { region.dealloc(ptr, layout) };
        }
    }

    /// Resizes for a thread that has no heap yet.
    #[cold]
    #[inline(never)]
    unsafe fn realloc_unbound(&self, ptr: NonNull<u8>, layout: Layout, new_size: usize) -> *mut u8 {
        if let Some(data) = self.bound_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and the caller vouches
            // for the block and the size.
            return unsafe { data.realloc(ptr, layout, new_size) };
        }
        let Some(region) = self.existing() else { return ptr::null_mut() };
        if let Some(data) = self.bind(region) {
            // SAFETY: binding claimed the heap for this thread, and the caller vouches for the
            // block and the size.
            return unsafe { data.realloc(ptr, layout, new_size) };
        }
        let resize = |heap: &Heap<'_>| {
            // SAFETY: the caller vouches for the block and the size.
            unsafe { heap.realloc(ptr, layout, new_size) }
        };
        region.heap().and_then(|heap| resize(&heap)).map_or(ptr::null_mut(), NonNull::as_ptr)
    }
}

// SAFETY: every method forwards to a heap or region of the allocator, which return blocks that
// are valid, aligned, disjoint and live until freed, and never unwind.
unsafe impl GlobalAlloc for Bcmalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(data) = self.thread_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone.
            return unsafe { data.alloc(layout) };
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_unbound(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if let Some(data) = self.thread_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone.
            return unsafe { data.alloc_zeroed(layout) };
        }
        // SAFETY: forwarded.
        let block = unsafe { self.alloc_unbound(layout) };
        if !block.is_null() {
            // SAFETY: the block holds the layout's size in bytes.
            unsafe { block.write_bytes(0, layout.size()) };
        }
        block
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees the block is live, so it is not null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        match self.thread_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and the caller
            // guarantees the block came from this allocator with this layout.
            Some(data) => unsafe { data.dealloc(ptr, layout) },
            // SAFETY: as above.
            None => unsafe { self.dealloc_unbound(ptr, layout) },
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees the block is live, so it is not null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        if let Some(data) = self.thread_heap() {
            // SAFETY: a thread's bound heap is claimed by that thread alone, and the caller
            // guarantees the block is live with this layout and that the new size is valid for
            // the alignment.
            return unsafe { data.realloc(ptr, layout, new_size) };
        }
        // SAFETY: as above.
        unsafe { self.realloc_unbound(ptr, layout, new_size) }
    }
}

#[cfg(all(test, not(loom), not(miri)))]
mod test {
    use super::*;
    use crate::config::Commit;
    use core::alloc::Layout;
    use std::{boxed::Box, vec::Vec};

    /// An allocator over a small region, leaked so that it can be used as a static would be.
    fn instance() -> &'static Bcmalloc {
        let config = Config::new().capacity(1 << 28).max_heaps(4).commit(Commit::Lazy);
        Box::leak(Box::new(Bcmalloc::with_config(config)))
    }

    #[test]
    fn collections_can_take_the_allocator_as_a_parameter() {
        let allocator = instance();
        // Joining a spawned thread waits for the operating system thread and so for the
        // destructor that releases its heap, which a scoped thread does not.
        let threads: Vec<_> = (0..4_u64)
            .map(|seed| {
                std::thread::spawn(move || {
                    let mut numbers = Vec::new_in(allocator);
                    numbers.extend((0..100_000).map(|i| i * seed));
                    assert_eq!(numbers[99_999], 99_999 * seed);
                    let boxed = Box::new_in([seed as u8; 64], allocator);
                    assert_eq!(boxed[63], seed as u8);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(allocator.stats().unwrap().heaps_in_use, 0);
    }

    #[test]
    fn a_zero_sized_request_does_not_create_the_region() {
        let allocator = instance();
        let empty = allocator.allocate(Layout::from_size_align(0, 32).unwrap()).unwrap();
        assert_eq!(empty.len(), 0);
        // SAFETY: the block came from this allocator with this layout.
        unsafe { allocator.deallocate(empty.cast(), Layout::from_size_align(0, 32).unwrap()) };
        assert!(allocator.stats().is_none());
    }

    #[test]
    fn a_block_can_be_freed_and_resized_with_the_layout_the_allocator_reports() {
        let allocator = instance();
        for (size, align) in [(1, 1), (100, 8), (5000, 64), (300_000, 4096), (3 << 20, 16)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            // SAFETY: the layout has a non-zero size.
            let block = NonNull::new(unsafe { allocator.alloc(layout) }).unwrap();
            let held = allocator.block_layout(block).unwrap();
            assert!(held.size() >= size, "{size} fits in what the allocator holds");
            // SAFETY: the block is live, and the reported layout is valid for resizing it.
            let grown = unsafe { allocator.realloc(block.as_ptr(), held, held.size() * 2) };
            let grown = NonNull::new(grown).unwrap();
            let held = allocator.block_layout(grown).unwrap();
            assert!(held.size() >= size * 2);
            // SAFETY: the block is live, and the reported layout is valid for freeing it.
            unsafe { allocator.dealloc(grown.as_ptr(), held) };
        }
        let outside = NonNull::from(&0_u64).cast::<u8>();
        assert_eq!(allocator.block_layout(outside), None, "a pointer outside the region");
    }
    #[test]
    fn frees_without_a_layout_return_everything_from_any_thread() {
        let allocator = instance();
        let sizes = [1, 100, 5000, 300_000, 3 << 20];
        let blocks: Vec<_> = sizes
            .iter()
            .map(|&size| {
                let layout = Layout::from_size_align(size, 16).unwrap();
                // SAFETY: the layout has a non-zero size.
                NonNull::new(unsafe { allocator.alloc(layout) }).unwrap().addr().get()
            })
            .collect();
        let (mine, theirs) = blocks.split_at(2);
        let theirs = theirs.to_vec();
        // A fresh thread has no heap, so its first free takes the path that binds one.
        std::thread::spawn(move || {
            for &addr in &theirs {
                // SAFETY: each block is live and freed once.
                unsafe { allocator.free_unsized(NonNull::new(addr as *mut u8).unwrap()) };
            }
        })
        .join()
        .unwrap();
        let mut outside = 0_u8;
        // SAFETY: as above, and the last pointer is not in the region.
        unsafe {
            for &addr in mine {
                allocator.free_unsized(NonNull::new(addr as *mut u8).unwrap());
            }
            allocator.free_unsized(NonNull::from(&mut outside));
        }
        allocator.flush_thread();
        allocator.trim();
        let stats = allocator.stats().unwrap();
        assert_eq!(stats.free, stats.capacity, "every block went back");
    }

    #[test]
    fn malloc_serves_every_size_aligned_and_refuses_impossible_ones() {
        let allocator = instance();
        let sizes = [0, 1, 15, 16, 17, 8191, 8192, 8193, 100_000, 600_000, 5 << 20];
        let blocks: Vec<_> = sizes.iter().map(|&size| (allocator.malloc(size), size)).collect();
        for &(block, size) in &blocks {
            assert!(!block.is_null(), "{size} bytes");
            assert!(block.addr().is_multiple_of(16), "{size} bytes are aligned to 16");
            let held = allocator.block_layout(NonNull::new(block).unwrap()).unwrap();
            assert!(held.size() >= size.max(1), "{size} bytes fit in the block");
            // SAFETY: the block holds the size in bytes.
            unsafe { block.write_bytes(0xa5, size) };
        }
        let distinct: std::collections::BTreeSet<_> = blocks.iter().map(|&(b, _)| b).collect();
        assert_eq!(distinct.len(), blocks.len(), "a zero-sized request gets a block of its own");
        assert!(allocator.malloc(usize::MAX).is_null());
        assert!(allocator.malloc(usize::MAX / 2).is_null());
        for (block, _) in blocks {
            // SAFETY: each block is live and freed once.
            unsafe { allocator.free_unsized(NonNull::new(block).unwrap()) };
        }
    }
    #[test]
    fn c_style_resizing_and_sizes_need_no_layout() {
        let allocator = instance();
        let block = NonNull::new(allocator.malloc(100)).unwrap();
        assert_eq!(allocator.usable_size(block), Some(112));
        // SAFETY: the block holds 100 bytes.
        unsafe { block.as_ptr().write_bytes(7, 100) };
        // SAFETY: the block is live and the sizes are not zero.
        let same = unsafe { allocator.realloc_unsized(block, 60) };
        assert_eq!(same, block.as_ptr(), "a block that still fits stays");
        // SAFETY: as above.
        let grown = NonNull::new(unsafe { allocator.realloc_unsized(block, 300_000) }).unwrap();
        assert_ne!(grown, block);
        // SAFETY: the grown block holds the first 60 bytes of the old one.
        assert!(unsafe { core::slice::from_raw_parts(grown.as_ptr(), 60) }.iter().all(|&b| b == 7));
        assert_eq!(allocator.usable_size(grown), Some(5 * (64 << 10)));
        // SAFETY: as above.
        assert!(unsafe { allocator.realloc_unsized(grown, usize::MAX) }.is_null());
        let mut outside = 0_u8;
        assert_eq!(allocator.usable_size(NonNull::from(&mut outside)), None);
        // SAFETY: the block is live and freed once.
        unsafe { allocator.free_unsized(grown) };
    }
    #[test]
    fn a_thread_whose_first_call_resizes_or_zeroes_binds_a_heap() {
        let allocator = instance();
        let layout = Layout::from_size_align(100, 8).unwrap();
        // SAFETY: the layout is not zero sized.
        let block = unsafe { allocator.alloc(layout) }.addr();
        std::thread::spawn(move || {
            let block = ptr::with_exposed_provenance_mut::<u8>(block);
            // SAFETY: the block is live with this layout, and the new size is valid.
            let grown = unsafe { allocator.realloc(block, layout, 5000) };
            assert!(!grown.is_null());
            // SAFETY: the grown block is live with the new layout.
            unsafe { allocator.dealloc(grown, Layout::from_size_align(5000, 8).unwrap()) };
        })
        .join()
        .unwrap();
        std::thread::spawn(move || {
            let zeroed = Layout::from_size_align(3000, 16).unwrap();
            // SAFETY: the layout is not zero sized.
            let block = unsafe { allocator.alloc_zeroed(zeroed) };
            // SAFETY: the block holds 3000 bytes.
            assert!(unsafe { core::slice::from_raw_parts(block, 3000) }.iter().all(|&b| b == 0));
            // SAFETY: the block is live with this layout.
            unsafe { allocator.dealloc(block, zeroed) };
        })
        .join()
        .unwrap();
        assert_eq!(allocator.config().heap_count(), 4);
    }

    #[test]
    fn flushing_a_thread_returns_what_its_heap_kept() {
        let allocator = instance();
        std::thread::spawn(move || {
            let blocks: Vec<_> =
                (0..1000).map(|i| Box::new_in([i as u8; 100], allocator)).collect();
            drop(blocks);
            let held = allocator.stats().unwrap();
            assert!(held.free < held.capacity, "the heap keeps some blocks and a slab");
            allocator.flush_thread();
            let after = allocator.stats().unwrap();
            assert_eq!(after.free, after.capacity, "everything went back to the region");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_thread_that_alternates_between_allocators_keeps_one_heap_in_each() {
        let (first, second) = (instance(), instance());
        let layout = Layout::from_size_align(64, 8).unwrap();
        for _ in 0..20 {
            for allocator in [first, second] {
                // SAFETY: the layout is not zero sized, and the block is freed once.
                unsafe {
                    let block = allocator.alloc(layout);
                    assert!(!block.is_null());
                    allocator.dealloc(block, layout);
                }
            }
        }
        assert_eq!(first.stats().unwrap().heaps_in_use, 1);
        assert_eq!(second.stats().unwrap().heaps_in_use, 1);
    }
}
