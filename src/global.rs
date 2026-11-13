//! The `#[global_allocator]` front end.
//!
//! [`Cwmalloc`] is a `static` that creates its [`Region`] the first time anything allocates, and
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

use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::{self, NonNull},
};

use crate::{
    config::Config,
    heap::{Heap, HeapData},
    region::{Error, Mapping, Region, Stats},
    sync::{AtomicPtr, AtomicUsize, Ordering},
    tls,
};

/// The smallest region the lazy start-up settles for when the OS refuses a bigger one.
const MIN_CAPACITY: usize = 256 << 20;

/// The value of [`Cwmalloc`] key field before the key exists.
const NO_KEY: usize = usize::MAX;

/// A global allocator that reserves its memory up front.
///
/// Declare one as the program's allocator and everything else follows.
///
/// ```
/// use cwmalloc::Cwmalloc;
///
/// #[global_allocator]
/// static GLOBAL: Cwmalloc = Cwmalloc::new();
///
/// let numbers: Vec<u64> = (0..1000).collect();
/// assert_eq!(numbers.iter().sum::<u64>(), 499_500);
/// ```
///
/// The region is reserved on first use with the default [`Config`]. Use [`Cwmalloc::with_config`]
/// to size it for the program. If the operating system refuses the reservation, the allocator
/// retries with half the capacity, down to 256 MiB.
///
/// ```
/// use cwmalloc::{Commit, Config, Cwmalloc};
///
/// #[global_allocator]
/// static GLOBAL: Cwmalloc =
///     Cwmalloc::with_config(Config::new().capacity(8 << 30).max_heaps(256).commit(Commit::Lazy));
///
/// let text = String::from("allocated from an 8 GiB region");
/// assert_eq!(GLOBAL.stats().unwrap().heaps_in_use, 1);
/// # drop(text);
/// ```
#[derive(Debug)]
pub struct Cwmalloc {
    config: Config,
    region: AtomicPtr<Region<'static>>,
    /// The thread-specific key under which threads keep their heap, or `NO_KEY`.
    key: AtomicUsize,
}

impl Cwmalloc {
    /// An allocator with the default [`Config`].
    pub const fn new() -> Self {
        Self::with_config(Config::new())
    }

    /// An allocator with the given configuration.
    pub const fn with_config(config: Config) -> Self {
        Self { config, region: AtomicPtr::new(ptr::null_mut()), key: AtomicUsize::new(NO_KEY) }
    }

    /// The configuration the region was or will be created with.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// A snapshot of the region, or `None` if nothing has been allocated yet.
    pub fn stats(&self) -> Option<Stats> {
        self.existing().map(Region::stats)
    }

    /// Returns the memory of idle heaps and free chunks to the operating system. See
    /// [`Region::trim`] for what that costs.
    pub fn trim(&self) {
        if let Some(region) = self.existing() {
            region.trim();
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

impl Default for Cwmalloc {
    fn default() -> Self {
        Self::new()
    }
}

impl Cwmalloc {
    /// This thread's heap, or null if it has not bound one.
    #[inline(always)]
    fn thread_heap(&self) -> *const HeapData {
        let key = self.key.load(Ordering::Relaxed);
        if key == NO_KEY { ptr::null() } else { tls::get(key) }
    }

    /// The key under which threads bind their heap, created on first use.
    fn key(&self) -> Option<usize> {
        let key = self.key.load(Ordering::Acquire);
        if key != NO_KEY {
            return Some(key);
        }
        let created = tls::create_key()?;
        match self.key.compare_exchange(NO_KEY, created, Ordering::AcqRel, Ordering::Acquire) {
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
        if tls::exiting() {
            return None;
        }
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
unsafe impl GlobalAlloc for Cwmalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: a thread's bound heap is claimed by that thread alone.
        if let Some(data) = unsafe { self.thread_heap().as_ref() } {
            // SAFETY: as above.
            return unsafe { data.alloc(layout) };
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_unbound(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees the block is live, so it is not null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        // SAFETY: a thread's bound heap is claimed by that thread alone.
        match unsafe { self.thread_heap().as_ref() } {
            // SAFETY: the caller guarantees the block came from this allocator with this layout.
            Some(data) => unsafe { data.dealloc(ptr, layout) },
            // SAFETY: as above.
            None => unsafe { self.dealloc_unbound(ptr, layout) },
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees the block is live, so it is not null.
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        // SAFETY: a thread's bound heap is claimed by that thread alone.
        if let Some(data) = unsafe { self.thread_heap().as_ref() } {
            // SAFETY: the caller guarantees the block is live with this layout and that the new
            // size is valid for the alignment.
            return unsafe { data.realloc(ptr, layout, new_size) };
        }
        // SAFETY: as above.
        unsafe { self.realloc_unbound(ptr, layout, new_size) }
    }
}

#[cfg(all(test, not(loom), not(miri)))]
mod test {
    use core::alloc::Layout;
    use std::boxed::Box;

    use super::*;
    use crate::config::Commit;

    /// An allocator over a small region, leaked so that it can be used as a static would be.
    fn instance() -> &'static Cwmalloc {
        let config = Config::new().capacity(1 << 28).max_heaps(4).commit(Commit::Lazy);
        Box::leak(Box::new(Cwmalloc::with_config(config)))
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
