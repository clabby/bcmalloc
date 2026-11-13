//! The region, which is one contiguous reservation holding the metadata of the allocator and every
//! byte it hands out.
//!
//! Everything lives in a single mapping, laid out so that the zero-filled pages the kernel provides
//! are a valid initial state for all of it. Nothing is written at start-up, so creating a region of
//! any size is instant and costs nothing until it is used.
//!
//! ```text
//!   +--------+------------+-------------+-----------+---------+-----+--------------------------+
//!   | Region | heap slots | descriptors | chunk map | bitmaps | pad | chunks (managed memory)  |
//!   | header | max_heaps  | 128 B/chunk | 8 B/chunk | 4 bits  |     | 64 KiB each, 4 MiB align |
//!   +--------+------------+-------------+-----------+---------+-----+--------------------------+
//!   |<------------ metadata, about 0.2% of the region ------------>|<---- the other 99.8% --->|
//! ```
//!
//! Metadata sits next to the data it describes but never inside it. The descriptor of a block is
//! found by arithmetic on its address, and so is its entry in the chunk map, which records the heap
//! that owns each chunk and the size class of the slab it belongs to. That map is what TCMalloc and
//! snmalloc call a pagemap, and here it is a flat array because the region is one reservation. No
//! header is read from user memory, so a buffer overflow in the program cannot corrupt the
//! bookkeeping of the allocator, and freeing needs no lookup structure.
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
    slab::Slab,
    sync::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence},
    sys,
};

/// How often the purge clock ticks, in milliseconds, and how long free chunks must have gone
/// unused before they go back to the OS.
const PURGE_INTERVAL_MS: u64 = 10;

/// The alignment of the managed memory, which is the size of the largest slab.
const SLAB_ALIGN: usize = 64 * CHUNK_SIZE;

/// The chunks in one transparent huge page of 2 MiB, which is the unit the purge clock gives back
/// when the region asks for huge pages.
const HUGE_PAGE_CHUNKS: usize = (2 << 20) / CHUNK_SIZE;

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
    /// For every chunk of a slab, the owning heap and the class. See [`Entry`].
    map: &'a [AtomicU64],
    heaps: &'a [HeapData],
    /// How many heap slots may have been claimed. Slots at or beyond it are untouched, so the
    /// operations that visit every slot stop here.
    claimed_mark: AtomicUsize,
    /// When the purge clock next ticks, in milliseconds on the monotonic clock.
    next_purge: AtomicU64,
    /// Whether any free chunk may still be waiting to be purged or to age towards it.
    aging: AtomicBool,
    /// How many purge passes and trims are holding free chunks right now, which makes the chunks
    /// look allocated to everyone else.
    purges_active: AtomicUsize,
    /// How many purge passes and trims have finished.
    purges_done: AtomicUsize,
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
    ///   `chunk_count` unused descriptors, `map` `chunk_count` entries, and `heaps` unclaimed heap
    ///   slots.
    /// * `purge_granule` is how many chunks the purge clock gives back together, a power of two of
    ///   at most 64 that `base` is aligned to in chunks.
    pub(crate) unsafe fn from_parts(
        base: *mut u8,
        words: &'a [AtomicU64],
        chunk_count: usize,
        slabs: &'a [Slab],
        map: &'a [AtomicU64],
        heaps: &'a [HeapData],
        purge_granule: usize,
    ) -> Self {
        debug_assert_eq!(slabs.len(), chunk_count);
        debug_assert_eq!(map.len(), chunk_count);
        Self {
            base,
            chunks: Chunks::with_granule(words, chunk_count, purge_granule),
            slabs,
            map,
            heaps,
            claimed_mark: AtomicUsize::new(0),
            next_purge: AtomicU64::new(0),
            aging: AtomicBool::new(false),
            purges_active: AtomicUsize::new(0),
            purges_done: AtomicUsize::new(0),
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
    /// torn down. The block is sent to the heap that owns its slab, which takes it back the next
    /// time it looks at its inbox.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live block of this region allocated with exactly `layout`, and must not be
    /// used or freed again.
    pub unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        match classify(layout) {
            Some(Request::Small(_)) => {
                let owner = self.entry(self.chunk_index(ptr.as_ptr())).owner();
                // SAFETY: the caller gives the block up, and the chunk map names its owner.
                unsafe { self.heap_slot(owner).post_block(ptr.as_ptr().cast()) };
            }
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
        // Flushing one heap can send blocks to another that was flushed before it, so a few more
        // passes take in what arrived. Each pass sends less, because a heap only sends blocks of
        // other heaps and those come to rest in their own slabs.
        for _ in 0..4 {
            for (id, data) in self.heaps[..claimed].iter().enumerate() {
                if data.claim(id as u32, self) {
                    // SAFETY: we just claimed the slot.
                    unsafe { data.flush(self) };
                    data.release_claim();
                }
            }
            if self.heaps[..claimed].iter().all(|data| data.is_claimed() || !data.has_mail()) {
                break;
            }
        }
        self.purges_active.fetch_add(1, Ordering::Release);
        self.chunks.drain(|run| {
            // SAFETY: the run is free and held by the drain while we advise on it.
            unsafe {
                let start = NonNull::new_unchecked(self.chunk_ptr(run.start));
                sys::discard(start, run.len() << CHUNK_SHIFT);
            }
        });
        self.purges_done.fetch_add(1, Ordering::Release);
        self.purges_active.fetch_sub(1, Ordering::Release);
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
            None => self.alloc_chunks_after_purge(n, align, hint)?,
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

    /// Looks again for chunks after a failed claim, in case a purge pass was holding the free
    /// ones. The region is only reported full when no pass overlapped a failed attempt.
    #[cold]
    fn alloc_chunks_after_purge(&self, n: usize, align: usize, hint: usize) -> Option<usize> {
        loop {
            let active = self.purges_active.load(Ordering::Acquire);
            let done = self.purges_done.load(Ordering::Acquire);
            // Every round looks at the bitmap, so chunks that are free for some other reason can
            // be used even while a pass that was descheduled is still holding its own.
            if let Some(first) = self.chunks.alloc(n, align, hint) {
                return Some(first);
            }
            // The attempt may have failed because of chunks that a pass claimed. Seeing such a
            // claim synchronises with the pass, so after this fence its counters are visible.
            fence(Ordering::Acquire);
            if active == 0 &&
                self.purges_active.load(Ordering::Acquire) == 0 &&
                self.purges_done.load(Ordering::Acquire) == done
            {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Frees chunks as `Chunks`::free_at does, and starts them ageing towards a purge.
    pub(crate) fn free_chunks(&self, first: usize, n: usize) {
        self.chunks.free_at(first, n, sys::monotonic_ms());
        // A read-modify-write, so that the pass that consumes the flag also sees the bits above no
        // matter which free it read the flag from.
        self.aging.fetch_or(true, Ordering::Release);
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
        // Take the flag first so that a free that races with the pass sets it again. Taking it
        // acquires the bits of every free that set it.
        self.aging.swap(false, Ordering::Acquire);
        self.purges_active.fetch_add(1, Ordering::Release);
        let pending = self.chunks.purge_pass(now, PURGE_INTERVAL_MS, |run| {
            // SAFETY: the pass holds the run, so no allocation overlaps it.
            unsafe {
                sys::discard(
                    NonNull::new_unchecked(self.chunk_ptr(run.start)),
                    run.len() << CHUNK_SHIFT,
                );
            }
        });
        self.purges_done.fetch_add(1, Ordering::Release);
        self.purges_active.fetch_sub(1, Ordering::Release);
        if pending {
            self.aging.fetch_or(true, Ordering::Release);
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

    /// The heap slot with id `id`.
    pub(crate) const fn heap_slot(&self, id: u32) -> &HeapData {
        &self.heaps[id as usize]
    }

    /// The chunk map entry of chunk `index`.
    pub(crate) fn entry(&self, index: usize) -> Entry {
        Entry(self.map[index].load(Ordering::Relaxed))
    }

    /// The first chunk map entry, with provenance over the whole map.
    pub(crate) const fn map_ptr(&self) -> *const AtomicU64 {
        self.map.as_ptr()
    }

    /// Records that the `n` chunks from `first` hold a slab of `class` owned by heap `owner`.
    ///
    /// The entries are read by threads that free blocks of the slab. Those threads got the blocks
    /// from the owner through some synchronisation of their own, which carries these stores with
    /// it, so relaxed stores are enough.
    pub(crate) fn set_owner(&self, first: usize, n: usize, owner: u32, class: Class) {
        let entry = Entry::new(owner, class);
        for slot in &self.map[first..first + n] {
            slot.store(entry.0, Ordering::Relaxed);
        }
    }
}

/// What the chunk map records for a chunk: the heap that owns the slab and the slab's size class.
///
/// The owner sits in the low 32 bits, so the free path reads it with one load and a truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry(pub(crate) u64);

impl Entry {
    /// The entry for a slab of `class` owned by heap `owner`.
    pub(crate) const fn new(owner: u32, class: Class) -> Self {
        Self(owner as u64 | (class.index() as u64) << 32)
    }

    /// The id of the heap that owns the slab.
    pub(crate) const fn owner(self) -> u32 {
        self.0 as u32
    }

    /// The size class of the slab.
    pub(crate) const fn class(self) -> Class {
        Class::from_index((self.0 >> 32) as u8 as usize)
    }
}

/// The byte offsets of each part of a mapping.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    heaps: usize,
    slabs: usize,
    map: usize,
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
        let map_at = slabs_at.checked_add(chunks.checked_mul(mem::size_of::<Slab>())?)?;
        let words_at = map_at.checked_add(chunks.checked_mul(mem::size_of::<AtomicU64>())?)?;
        let words_end =
            words_at.checked_add(Chunks::storage_words(chunks) * mem::size_of::<AtomicU64>())?;
        let data = words_end.checked_next_multiple_of(SLAB_ALIGN)?;
        let total = data.checked_add(chunks.checked_mul(CHUNK_SIZE)?)?;
        Some(Self { heaps: heaps_at, slabs: slabs_at, map: map_at, words: words_at, data, total })
    }
}

/// The OS mapping behind a [`Region`]. Dropping it unmaps everything.
///
/// ```
/// use bcmalloc::{Config, Mapping};
///
/// let mapping = Mapping::new(&Config::new().capacity(64 << 20).max_heaps(8))?;
/// let region = mapping.region();
/// assert_eq!(region.stats().capacity, 64 << 20);
/// # Ok::<(), bcmalloc::Error>(())
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
            let map = slice::from_raw_parts(at(geometry.map).cast::<AtomicU64>(), chunks);
            let words = slice::from_raw_parts(
                at(geometry.words).cast::<AtomicU64>(),
                Chunks::storage_words(chunks),
            );
            // With huge pages, giving back less than a whole one would split it.
            let granule = if config.wants_huge_pages() && cfg!(target_os = "linux") {
                HUGE_PAGE_CHUNKS
            } else {
                1
            };
            let region = Region::from_parts(
                at(geometry.data),
                words,
                chunks,
                slabs,
                map,
                heap_slots,
                granule,
            );
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
    fn a_retry_does_not_wait_for_a_purge_pass_while_other_chunks_are_free() {
        use std::{sync::mpsc, time::Duration};

        use crate::testing::TestRegion;

        let test = TestRegion::new(2, 1);
        let region = test.region();
        let a = region.alloc_chunks(1, 1, 0).unwrap();
        region.alloc_chunks(1, 1, 0).unwrap();
        // A pass that was descheduled while holding some other chunks.
        region.purges_active.fetch_add(1, Ordering::Release);
        region.free_chunks(a, 1);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| sender.send(region.alloc_chunks_after_purge(1, 1, 0)).unwrap());
            let found = receiver.recv_timeout(Duration::from_secs(5));
            // Let the retry finish whatever happened, so that the scope can end.
            region.purges_active.fetch_sub(1, Ordering::Release);
            assert_eq!(found, Ok(Some(a)), "the retry waited for the pass");
        });
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

#[cfg(all(test, loom))]
mod loom_test {
    use loom::thread;

    use super::*;
    use crate::testing::TestRegion;

    #[test]
    fn a_free_racing_a_purge_pass_never_strands_a_dirty_chunk() {
        loom::model(|| {
            // The threads share one region value, because the purge state lives in it.
            let test: &'static TestRegion =
                std::boxed::Box::leak(std::boxed::Box::new(TestRegion::new(1, 1)));
            let region: &'static Region<'static> =
                std::boxed::Box::leak(std::boxed::Box::new(test.region()));
            let chunk = region.alloc_chunks(1, 1, 0).unwrap();
            let freer = thread::spawn(move || region.free_chunks(chunk, 1));
            let purger = thread::spawn(move || region.purge_pass(u64::MAX));
            freer.join().unwrap();
            purger.join().unwrap();
            // Whatever the pass missed has to be announced, or nothing would ever look again.
            let mut missed = 0;
            region.chunks.purge_pass(u64::MAX, 0, |run| missed += run.len());
            assert!(
                missed == 0 || region.aging.load(Ordering::Acquire),
                "a free chunk is dirty and nothing is going to purge it"
            );
        });
    }
}
