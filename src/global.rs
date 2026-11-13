//! The `#[global_allocator]` front end.
//!
//! [`Cwmalloc`] is a `static` that creates its [`Region`] the first time anything allocates, and
//! gives every thread a [`Heap`] of its own. After that a call looks like this.
//!
//! ```text
//!    alloc(layout)
//!         |
//!         v
//!    region initialised? --no--> reserve and set up the region (one thread wins, the rest unmap)
//!         | yes
//!         v
//!    this thread has a heap? --no--> claim one and keep it in thread-local storage
//!         | yes
//!         v
//!    Heap::alloc(layout)  ... the fast path, no atomics
//! ```
//!
//! # Thread start-up and teardown
//!
//! Thread-local storage in `std` never allocates, which is what makes it safe to use from inside an
//! allocator. The heap lives in a thread-local with a destructor, so a thread's heap goes back to
//! the pool when the thread exits. Other destructors may still allocate or free after that one
//! has run, and thread-local access fails at that point. Frees fall back to
//! [`Region::dealloc`], and allocations borrow a heap for just that one call.
//!
//! # Failure
//!
//! A failed allocation returns null, and the standard library turns that into an abort. That
//! happens when the region is full, when the alignment is above [`MAX_ALIGN`](crate::MAX_ALIGN),
//! or when more threads are running than the configuration has heaps for.

use core::{
    alloc::{GlobalAlloc, Layout},
    cell::OnceCell,
    ptr::{self, NonNull},
};

use crate::{
    config::Config,
    heap::Heap,
    region::{Error, Mapping, Region, Stats},
    sync::{AtomicPtr, Ordering},
};

/// The smallest region the lazy start-up settles for when the OS refuses a bigger one.
const MIN_CAPACITY: usize = 256 << 20;

thread_local! {
    /// This thread's heap, claimed on first use and released when the thread exits.
    static HEAP: OnceCell<Heap<'static>> = const { OnceCell::new() };
}

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
}

impl Cwmalloc {
    /// An allocator with the default [`Config`].
    pub const fn new() -> Self {
        Self::with_config(Config::new())
    }

    /// An allocator with the given configuration.
    pub const fn with_config(config: Config) -> Self {
        Self { config, region: AtomicPtr::new(ptr::null_mut()) }
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

/// Runs `f` with this thread's heap, claiming one if needed.
///
/// Returns `None` when the thread has no usable heap, which happens during thread teardown, when
/// every heap is taken, or when the thread's heap belongs to a different region.
#[inline]
fn with_heap<R>(
    region: &'static Region<'static>,
    f: impl FnOnce(&Heap<'static>) -> R,
) -> Option<R> {
    HEAP.try_with(|cell| {
        if cell.get().is_none() {
            if let Some(heap) = region.heap() {
                let _ = cell.set(heap);
            }
        }
        cell.get().filter(|heap| ptr::eq(heap.region(), region)).map(f)
    })
    .ok()
    .flatten()
}

// SAFETY: every method forwards to a heap or region of the allocator, which return blocks that
// are valid, aligned, disjoint and live until freed, and never unwind.
unsafe impl GlobalAlloc for Cwmalloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(region) = self.region() else { return ptr::null_mut() };
        let block = with_heap(region, |heap| heap.alloc(layout))
            .unwrap_or_else(|| region.heap().and_then(|heap| heap.alloc(layout)));
        block.map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (Some(region), Some(ptr)) = (self.existing(), NonNull::new(ptr)) else { return };
        let freed = with_heap(region, |heap| {
            // SAFETY: the caller guarantees the block came from this allocator with this layout.
            unsafe { heap.dealloc(ptr, layout) }
        });
        if freed.is_none() {
            // SAFETY: as above.
            unsafe { region.dealloc(ptr, layout) };
        }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let (Some(region), Some(block)) = (self.existing(), NonNull::new(ptr)) else {
            return ptr::null_mut();
        };
        let resize = |heap: &Heap<'_>| {
            // SAFETY: the caller guarantees the block is live with this layout and that the new
            // size is valid for the alignment.
            unsafe { heap.realloc(block, layout, new_size) }
        };
        let resized = with_heap(region, resize)
            .unwrap_or_else(|| region.heap().and_then(|heap| resize(&heap)));
        resized.map_or(ptr::null_mut(), NonNull::as_ptr)
    }
}
