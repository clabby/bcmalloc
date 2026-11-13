//! The region, which is one contiguous reservation holding the metadata of the allocator and every
//! byte it hands out.
//!
//! Everything lives in a single mapping, laid out so that the zero-filled pages the kernel provides
//! are a valid initial state for all of it. Nothing is written at start-up, so creating a region of
//! any size is instant and costs nothing until it is used.
//!
//! ```text
//!   +--------+------------+------------------+--------+-----+-----------------------------+
//!   | Region | heap slots | slab descriptors | bitmap | pad | chunks (the managed memory) |
//!   | header | max_heaps  | 128 B per chunk  | 1 bit  |     | 64 KiB each, 4 MiB aligned  |
//!   +--------+------------+------------------+--------+-----+-----------------------------+
//!   |<-------- metadata, about 0.2% of the region -------->|<----- the other 99.8% ------>|
//! ```
//!
//! Metadata sits next to the data it describes but never inside it. The descriptor of a block is
//! found by arithmetic on its address. No header is read from user memory, so a buffer overflow in
//! the program cannot corrupt the bookkeeping of the allocator, and freeing needs no lookup
//! structure.
//!
//! A [`Region`] is the shared, thread-safe half of the allocator. Threads take a [`Heap`] from it
//! to allocate. A [`Mapping`] owns the memory behind a region and unmaps it on drop.

use core::{
    alloc::Layout,
    fmt, mem,
    ptr::{self, NonNull},
    slice,
};

use crate::{
    chunks::{CHUNK_SHIFT, CHUNK_SIZE, Chunks},
    config::{Commit, Config},
    heap::{Heap, HeapData},
    size_class::{Class, Request, classify},
    slab::{Block, Slab},
    sync::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sys,
};

/// How often the purge clock ticks, in milliseconds, and how long free chunks must have gone
/// unused before they go back to the OS.
const PURGE_INTERVAL_MS: u64 = 10;

/// The alignment of the managed memory, which is the size of the largest slab.
const SLAB_ALIGN: usize = 64 * CHUNK_SIZE;

/// Why a region could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The capacity is smaller than one chunk, there are no heaps, or the sizes overflow.
    InvalidConfig,
    /// The operating system refused to reserve the address space.
    OutOfAddressSpace,
    /// [`Commit::Lock`] was requested and the kernel would not pin the memory.
    LockFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidConfig => "invalid region configuration",
            Self::OutOfAddressSpace => "the operating system refused to reserve the region",
            Self::LockFailed => "the operating system refused to pin the region in memory",
        })
    }
}

impl core::error::Error for Error {}

/// A snapshot of a region's occupancy, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// The size of the managed memory.
    pub capacity: usize,
    /// How much of it is not currently part of any slab or huge allocation. Memory inside slabs
    /// that is free but not yet returned is not counted.
    pub free: usize,
    /// How many heaps are claimed right now.
    pub heaps_in_use: usize,
}

/// The shared state of an allocator, made of the chunk bitmap, the slab descriptors and the heap
/// slots.
///
/// A region is thread-safe. Threads allocate by taking a [`Heap`] from it with [`Region::heap`].
#[derive(Debug)]
pub struct Region<'a> {
    base: *mut u8,
    chunks: Chunks<'a>,
    slabs: &'a [Slab],
    heaps: &'a [HeapData],
    /// How many heap slots may have been claimed. Slots at or beyond it are untouched, so the
    /// operations that visit every slot stop here.
    claimed_mark: AtomicUsize,
    /// When the purge clock next ticks, in milliseconds on the monotonic clock.
    next_purge: AtomicU64,
    /// Whether any free chunk may still be waiting to be purged or to age towards it.
    aging: AtomicBool,
    /// How many purge passes and trims are holding free chunks, which makes them look allocated.
    purging: AtomicUsize,
}

// SAFETY: the raw pointer is an address into memory the region owns, and every shared structure
// behind the references is either atomic or handed out under an exclusive claim.
unsafe impl Send for Region<'_> {}
// SAFETY: as above.
unsafe impl Sync for Region<'_> {}

impl<'a> Region<'a> {
    /// Assembles a region from its parts.
    ///
    /// # Safety
    ///
    /// * `base` must point to `chunk_count` chunks of writable memory aligned to 4 MiB, valid for
    ///   as long as `'a`, and used by nothing else.
    /// * `words` must be zeroed bitmap storage for `chunk_count` chunks, `slabs` must hold
    ///   `chunk_count` unused descriptors, and `heaps` unclaimed heap slots.
    pub(crate) unsafe fn from_parts(
        base: *mut u8,
        words: &'a [AtomicU64],
        chunk_count: usize,
        slabs: &'a [Slab],
        heaps: &'a [HeapData],
    ) -> Self {
        debug_assert_eq!(slabs.len(), chunk_count);
        Self {
            base,
            chunks: Chunks::new(words, chunk_count),
            slabs,
            heaps,
            claimed_mark: AtomicUsize::new(0),
            next_purge: AtomicU64::new(0),
            aging: AtomicBool::new(false),
            purging: AtomicUsize::new(0),
        }
    }

    /// Claims a free heap, or returns `None` if every one is in use.
    pub fn heap(&self) -> Option<Heap<'_>> {
        // The search stops at the first free slot, so it only ever touches one slot that has never
        // been used. Bounding it by `claimed_mark` instead would make concurrent claims race for
        // that one slot and fail although others are free.
        for (id, data) in self.heaps.iter().enumerate() {
            if data.claim(id as u32, self) {
                self.claimed_mark.fetch_max(id + 1, Ordering::Release);
                return Some(Heap::new(self, data));
            }
        }
        None
    }

    /// Frees a block without a heap, from any thread.
    ///
    /// This is what a thread uses when it has no heap of its own, for instance because it is being
    /// torn down. The block is handed back to the slab that owns it, and that slab's heap reclaims
    /// it the next time it looks.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live block of this region allocated with exactly `layout`, and must not be
    /// used or freed again.
    pub unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        match classify(layout) {
            // SAFETY: forwarded.
            Some(Request::Small(class)) => unsafe { self.free_remote(ptr, class) },
            Some(Request::Huge { chunks }) => {
                self.free_chunks(self.chunk_index(ptr.as_ptr()), chunks);
                self.purge_if_due();
            }
            None => debug_assert!(false, "layout was never allocated by this region"),
        }
    }

    /// Returns the empty slabs of every idle heap to the region, and tells the operating system
    /// that free chunks no longer need physical pages.
    ///
    /// Heaps that are in use are skipped, and a thread trims its own slabs with [`Heap::flush`].
    /// While this drains a bitmap word the 64 chunks in it look allocated to everyone else, so run
    /// it when the program is not close to full.
    pub fn trim(&self) {
        let claimed = self.claimed_mark.load(Ordering::Acquire);
        for (id, data) in self.heaps[..claimed].iter().enumerate() {
            if data.claim(id as u32, self) {
                // SAFETY: we just claimed the slot.
                unsafe { data.flush(self) };
                data.release_claim();
            }
        }
        self.purging.fetch_add(1, Ordering::Release);
        self.chunks.drain(|run| {
            // SAFETY: the run is free and held by the drain while we advise on it.
            unsafe {
                let start = NonNull::new_unchecked(self.chunk_ptr(run.start));
                sys::discard(start, run.len() << CHUNK_SHIFT);
            }
        });
        self.purging.fetch_sub(1, Ordering::Release);
    }

    /// A snapshot of how full the region is.
    pub fn stats(&self) -> Stats {
        let claimed = self.claimed_mark.load(Ordering::Acquire);
        Stats {
            capacity: self.chunks.len() << CHUNK_SHIFT,
            free: self.chunks.free_chunks() << CHUNK_SHIFT,
            heaps_in_use: self.heaps[..claimed].iter().filter(|heap| heap.is_claimed()).count(),
        }
    }

    /// Whether `ptr` points into this region's managed memory.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let offset = ptr.addr().wrapping_sub(self.base.addr());
        offset < self.chunks.len() << CHUNK_SHIFT
    }

    /// Claims `n` chunks as `Chunks`::alloc does, and makes the pages of any that were given back
    /// to the OS ready for use.
    pub(crate) fn alloc_chunks(&self, n: usize, align: usize, hint: usize) -> Option<usize> {
        let first = match self.chunks.alloc(n, align, hint) {
            Some(first) => first,
            None => {
                // Free chunks that a purge pass holds look allocated for a moment. Wait for the
                // pass to finish and look again before reporting that the region is full.
                if self.purging.load(Ordering::Acquire) == 0 {
                    return None;
                }
                while self.purging.load(Ordering::Acquire) != 0 {
                    core::hint::spin_loop();
                }
                self.chunks.alloc(n, align, hint)?
            }
        };
        self.chunks.take_purged(first, n, |run| {
            // SAFETY: the run is inside the region and belongs to us since the claim above.
            unsafe {
                sys::reuse(
                    NonNull::new_unchecked(self.chunk_ptr(run.start)),
                    run.len() << CHUNK_SHIFT,
                );
            }
        });
        Some(first)
    }

    /// Frees chunks as `Chunks`::free_at does, and starts them ageing towards a purge.
    pub(crate) fn free_chunks(&self, first: usize, n: usize) {
        self.chunks.free_at(first, n, sys::monotonic_ms());
        self.aging.store(true, Ordering::Relaxed);
    }

    /// Runs the purge clock if free chunks are ageing and an interval has passed. Chunks that stay
    /// free for one to two intervals go back to the OS. This is called from paths that are already
    /// slow, so that an idle program pays nothing and a busy one pays a clock read now and then.
    #[inline]
    pub(crate) fn purge_if_due(&self) {
        if self.aging.load(Ordering::Relaxed) {
            self.purge_when_due();
        }
    }

    #[cold]
    fn purge_when_due(&self) {
        let now = sys::monotonic_ms();
        let due = self.next_purge.load(Ordering::Relaxed);
        let won = now >= due &&
            self.next_purge
                .compare_exchange(
                    due,
                    now + PURGE_INTERVAL_MS,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok();
        if won {
            self.purge_pass(now);
        }
    }

    /// One tick of the purge clock at time `now` in milliseconds.
    pub(crate) fn purge_pass(&self, now: u64) {
        // Clear the flag first so that a free that races with the pass sets it again.
        self.aging.store(false, Ordering::Relaxed);
        self.purging.fetch_add(1, Ordering::Release);
        let pending = self.chunks.purge_pass(now, PURGE_INTERVAL_MS, |run| {
            // SAFETY: the pass holds the run, so no allocation overlaps it.
            unsafe {
                sys::discard(
                    NonNull::new_unchecked(self.chunk_ptr(run.start)),
                    run.len() << CHUNK_SHIFT,
                );
            }
        });
        self.purging.fetch_sub(1, Ordering::Release);
        if pending {
            self.aging.store(true, Ordering::Relaxed);
        }
    }

    /// The descriptor stored for the slab whose first chunk is chunk `first`.
    pub(crate) const fn slab(&self, first: usize) -> &Slab {
        &self.slabs[first]
    }

    /// The first descriptor, with provenance over the whole table.
    pub(crate) const fn slabs_ptr(&self) -> *const Slab {
        self.slabs.as_ptr()
    }

    /// The address of chunk `index`.
    pub(crate) fn chunk_ptr(&self, index: usize) -> *mut u8 {
        debug_assert!(index < self.chunks.len());
        self.base.wrapping_add(index << CHUNK_SHIFT)
    }

    /// The index of the chunk containing `ptr`.
    pub(crate) fn chunk_index(&self, ptr: *const u8) -> usize {
        let offset = ptr.addr() - self.base.addr();
        debug_assert!(offset >> CHUNK_SHIFT < self.chunks.len(), "pointer outside the region");
        offset >> CHUNK_SHIFT
    }

    /// The descriptor of the slab of `class` that contains `ptr`.
    pub(crate) fn slab_of(&self, ptr: NonNull<u8>, class: Class) -> &Slab {
        // Slabs are aligned to their own length in chunks, so masking finds the first chunk.
        self.slab(self.chunk_index(ptr.as_ptr()) & !(class.slab_chunks() - 1))
    }

    /// Frees a small block by pushing it onto its slab's remote list.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live block of this region of `class`, and must not be used again.
    pub(crate) unsafe fn free_remote(&self, ptr: NonNull<u8>, class: Class) {
        let block = ptr.as_ptr().cast::<Block>();
        // SAFETY: a single block is a chain whose head and tail are the same.
        unsafe { self.free_remote_chain(self.slab_of(ptr, class), block, block) };
    }

    /// Frees a chain of blocks of `slab`, which the caller has linked from `head` to `tail`, by
    /// pushing them onto the slab's remote list in one step.
    ///
    /// # Safety
    ///
    /// The blocks must satisfy the contract of `push_remote_chain` on the slab.
    pub(crate) unsafe fn free_remote_chain(&self, slab: &Slab, head: *mut Block, tail: *mut Block) {
        // SAFETY: forwarded.
        if unsafe { slab.push_remote_chain(head, tail) } {
            // We turned the slab's full mark into a notification. The slab cannot be recycled until
            // its owner processes that notification, so the descriptor is still ours to read.
            slab.enqueue(self.heaps[slab.owner() as usize].unfull());
        }
    }
}

/// The byte offsets of each part of a mapping.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    heaps: usize,
    slabs: usize,
    words: usize,
    data: usize,
    total: usize,
}

impl Geometry {
    /// Lays out a region of `chunks` chunks and `heaps` heap slots, or None on overflow.
    fn new(chunks: usize, heaps: usize) -> Option<Self> {
        let heaps_at =
            mem::size_of::<Region<'_>>().checked_next_multiple_of(mem::align_of::<HeapData>())?;
        let slabs_at = heaps_at.checked_add(heaps.checked_mul(mem::size_of::<HeapData>())?)?;
        let words_at = slabs_at.checked_add(chunks.checked_mul(mem::size_of::<Slab>())?)?;
        let words_end =
            words_at.checked_add(Chunks::storage_words(chunks) * mem::size_of::<AtomicU64>())?;
        let data = words_end.checked_next_multiple_of(SLAB_ALIGN)?;
        let total = data.checked_add(chunks.checked_mul(CHUNK_SIZE)?)?;
        Some(Self { heaps: heaps_at, slabs: slabs_at, words: words_at, data, total })
    }
}

/// The OS mapping behind a [`Region`]. Dropping it unmaps everything.
///
/// ```
/// use cwmalloc::{Config, Mapping};
///
/// let mapping = Mapping::new(&Config::new().capacity(64 << 20).max_heaps(8))?;
/// let region = mapping.region();
/// assert_eq!(region.stats().capacity, 64 << 20);
/// # Ok::<(), cwmalloc::Error>(())
/// ```
#[derive(Debug)]
pub struct Mapping {
    /// The whole reservation, as the OS returned it.
    raw: NonNull<u8>,
    raw_len: usize,
    /// The 4 MiB-aligned start of the metadata, where the [`Region`] header lives.
    base: NonNull<u8>,
}

// SAFETY: a mapping is an owned reservation and the region inside is Sync.
unsafe impl Send for Mapping {}
// SAFETY: as above.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Reserves and sets up a region as described by `config`.
    ///
    /// # Errors
    ///
    /// Fails if the configuration is unusable, the operating system refuses the reservation, or
    /// [`Commit::Lock`] cannot be honoured.
    pub fn new(config: &Config) -> Result<Self, Error> {
        let chunks = config.capacity_bytes() / CHUNK_SIZE;
        let heaps = config.heap_count();
        if chunks == 0 || heaps == 0 {
            return Err(Error::InvalidConfig);
        }
        let geometry = Geometry::new(chunks, heaps).ok_or(Error::InvalidConfig)?;
        let raw_len = geometry.total.checked_add(SLAB_ALIGN).ok_or(Error::InvalidConfig)?;
        let raw = sys::reserve(raw_len).ok_or(Error::OutOfAddressSpace)?;

        // The kernel only promises page alignment and slabs need more. The slack is never touched.
        let pad = raw.as_ptr().addr().wrapping_neg() & (SLAB_ALIGN - 1);
        // SAFETY: the reservation is `SLAB_ALIGN` longer than the layout needs.
        let base = unsafe { NonNull::new_unchecked(raw.as_ptr().add(pad)) };
        let mapping = Self { raw, raw_len, base };

        // SAFETY: every offset in `geometry` lies inside the zeroed reservation, with the alignment
        // its type needs, and an all-zero value is valid for each of the types.
        unsafe {
            let at = |offset: usize| base.as_ptr().add(offset);
            let heap_slots = slice::from_raw_parts(at(geometry.heaps).cast::<HeapData>(), heaps);
            let slabs = slice::from_raw_parts(at(geometry.slabs).cast::<Slab>(), chunks);
            let words = slice::from_raw_parts(
                at(geometry.words).cast::<AtomicU64>(),
                Chunks::storage_words(chunks),
            );
            let region = Region::from_parts(at(geometry.data), words, chunks, slabs, heap_slots);
            ptr::write(base.as_ptr().cast::<Region<'static>>(), region);
        }

        mapping.apply(config, &geometry)?;
        Ok(mapping)
    }

    /// The region this mapping backs.
    pub const fn region(&self) -> &Region<'_> {
        // SAFETY: `new` wrote a region at `base`, and it lives as long as the mapping.
        unsafe { self.base.cast::<Region<'static>>().as_ref() }
    }

    /// Gives up ownership and returns the region for the life of the program.
    #[cfg(all(feature = "std", not(loom)))]
    pub(crate) const fn leak(self) -> &'static Region<'static> {
        // SAFETY: as in `region`, and forgetting the mapping means it is never unmapped.
        let region = unsafe { self.base.cast::<Region<'static>>().as_ref() };
        mem::forget(self);
        region
    }

    /// Applies the OS-level settings in `config`.
    fn apply(&self, config: &Config, geometry: &Geometry) -> Result<(), Error> {
        let data_len = config.capacity_bytes();
        // SAFETY: the data range is inside the reservation.
        let data = unsafe { NonNull::new_unchecked(self.base.as_ptr().add(geometry.data)) };
        if config.wants_huge_pages() {
            // SAFETY: as above.
            unsafe { sys::advise_huge(data, data_len) };
        }
        match config.commit_mode() {
            Commit::Lazy => {}
            Commit::Populate | Commit::Lock => {
                // SAFETY: the data range is inside the reservation and holds nothing yet.
                unsafe { sys::populate(data, data_len) };
                if config.commit_mode() == Commit::Lock {
                    // SAFETY: the whole metadata-plus-data range is inside the reservation.
                    let locked = unsafe { sys::lock(self.base, geometry.total) };
                    if !locked {
                        return Err(Error::LockFailed);
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the pair is exactly what `reserve` returned, and borrowing rules forbid any heap
        // outliving the mapping.
        unsafe { sys::release(self.raw, self.raw_len) };
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;

    const MIB: usize = 1 << 20;

    const fn config(capacity: usize) -> Config {
        Config::new().capacity(capacity).max_heaps(4)
    }

    #[test]
    fn a_mapping_backs_a_working_region() {
        let mapping = Mapping::new(&config(64 * MIB)).unwrap();
        let region = mapping.region();
        assert_eq!(region.stats(), Stats { capacity: 64 * MIB, free: 64 * MIB, heaps_in_use: 0 });

        let heap = region.heap().unwrap();
        let layout = Layout::from_size_align(1000, 16).unwrap();
        let block = heap.alloc(layout).unwrap();
        assert!(region.contains(block.as_ptr()));
        assert_eq!(region.stats().heaps_in_use, 1);
        // SAFETY: allocated just above with this layout.
        unsafe { heap.dealloc(block, layout) };
    }

    #[test]
    fn slabs_start_on_multiples_of_their_size() {
        let mapping = Mapping::new(&config(64 * MIB)).unwrap();
        let heap = mapping.region().heap().unwrap();
        // The largest class lives in 4 MiB slabs, and its blocks are 512 KiB apart.
        let layout = Layout::from_size_align(512 << 10, 64 << 10).unwrap();
        let blocks: std::vec::Vec<_> = (0..4).map(|_| heap.alloc(layout).unwrap()).collect();
        assert!(blocks.iter().all(|block| block.as_ptr().addr() % (512 << 10) == 0));
    }

    #[test]
    fn allocation_never_fails_because_a_purge_pass_holds_the_free_chunks() {
        use crate::testing::{TestRegion, iterations};

        // Two threads hold at most two of the four chunks, so two are always free. A pass that
        // holds both for a moment must not make an allocation report that the region is full.
        let test = TestRegion::new(4, 1);
        let region = test.region();
        let rounds = iterations(20_000);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    for _ in 0..rounds {
                        let chunk = region.alloc_chunks(1, 1, 0).expect("a chunk is free");
                        region.free_chunks(chunk, 1);
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..rounds {
                    region.purge_pass(u64::MAX);
                }
            });
        });
    }

    #[test]
    fn unusable_configurations_are_rejected() {
        let rejected = |config: Config| Mapping::new(&config).unwrap_err();
        assert_eq!(rejected(Config::new().capacity(0)), Error::InvalidConfig);
        assert_eq!(rejected(Config::new().capacity(CHUNK_SIZE - 1)), Error::InvalidConfig);
        assert_eq!(rejected(config(MIB).max_heaps(0)), Error::InvalidConfig);
        assert_eq!(rejected(Config::new().capacity(usize::MAX)), Error::InvalidConfig);
    }

    #[test]
    #[cfg(not(miri))]
    fn a_reservation_the_os_refuses_is_an_error_not_a_panic() {
        // A petabyte is far beyond any user address space.
        assert_eq!(
            Mapping::new(&Config::new().capacity(1 << 50)).unwrap_err(),
            Error::OutOfAddressSpace
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn regions_are_not_limited_to_32_bit_offsets() {
        // 8 GiB of address space, with 5 GiB of it handed out as one allocation. Only the pages
        // that are touched cost memory.
        let mapping = Mapping::new(&config(8 << 30)).unwrap();
        let heap = mapping.region().heap().unwrap();
        let layout = Layout::from_size_align(5 << 30, 1).unwrap();
        let block = heap.alloc(layout).expect("5 GiB fits in an 8 GiB region");
        let (first, last) = (block.as_ptr(), block.as_ptr().wrapping_add((5 << 30) - 1));
        // SAFETY: both addresses are inside the live block.
        unsafe {
            first.write(1);
            last.write(2);
            assert_eq!((first.read(), last.read()), (1, 2));
            heap.dealloc(block, layout);
        }
        assert_eq!(mapping.region().stats().free, 8 << 30);
    }

    #[test]
    fn eager_commit_modes_produce_a_working_region() {
        for commit in [Commit::Populate, Commit::Lock] {
            match Mapping::new(&config(16 * MIB).commit(commit)) {
                Ok(mapping) => {
                    assert!(mapping.region().heap().unwrap().alloc(Layout::new::<u64>()).is_some())
                }
                // Pinning memory needs a high enough RLIMIT_MEMLOCK, which CI cannot promise.
                Err(error) => assert_eq!((commit, error), (Commit::Lock, Error::LockFailed)),
            }
        }
    }

    #[test]
    fn huge_page_advice_is_harmless() {
        let mapping = Mapping::new(&config(16 * MIB).huge_pages(true)).unwrap();
        assert!(mapping.region().heap().unwrap().alloc(Layout::new::<u64>()).is_some());
    }
}
