//! The region, which is one contiguous reservation holding the metadata of the allocator and every
//! byte it hands out.
//!
//! Everything lives in a single mapping, laid out so that the zero-filled pages the kernel provides
//! are a valid initial state for all of it. Creating a region writes its header and, unless the
//! configuration asks to populate the memory up front, nothing else, so a region of any size costs
//! address space until it is used.
//!
//! ```text
//!   +--------+-------+-----------+-------------+-----------+---------+-----+-------------------+
//!   | Region | heap  | bin slots | descriptors | chunk map | bitmaps | pad | chunks (managed   |
//!   | header | slots | per heap  | 640 B/chunk | 8 B/chunk | 4 bits  |     | memory), 64 KiB   |
//!   +--------+-------+-----------+-------------+-----------+---------+-----+-------------------+
//!   |<--------------- metadata, about 1% of the region ---------------->|<-- the other 99% ->|
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

use crate::{
    cache::CacheLine,
    chunks::{CHUNK_SHIFT, CHUNK_SIZE, Chunks},
    config::{Commit, Config},
    heap::{Heap, HeapData},
    size_class::{BIN_SLOTS, Class, Request, classify},
    slab::{Block, Slab},
    sync::{AtomicBool, AtomicU64, AtomicUsize, Ordering, fence},
    sys,
};
use core::{
    alloc::Layout,
    fmt, mem,
    ops::Range,
    ptr::{self, NonNull},
    slice,
};

/// How often the purge clock ticks, in milliseconds, and how long free chunks must have gone
/// unused before they go back to the OS.
const PURGE_INTERVAL_MS: u64 = 10;

/// The alignment of the managed memory: one bitmap word of 64 chunks, 4 MiB. Every word then covers
/// two whole huge pages, and every slab, which is at most 8 chunks, starts on a multiple of its
/// size.
const SLAB_ALIGN: usize = 64 * CHUNK_SIZE;

/// How many times longer free chunks of the zone of transparent huge pages wait before they are
/// given back. A huge page that is given back and then used again costs a fault that clears 2 MiB,
/// and big blocks that a program frees and allocates in turn would pay it every time, so idle huge
/// pages wait 100 ms, as jemalloc lets dirty pages decay instead of returning them at once.
const HUGE_PURGE_WAIT: u64 = 10;

/// The chunks in one transparent huge page of 2 MiB.
pub(crate) const HUGE_PAGE_CHUNKS: usize = (2 << 20) / CHUNK_SIZE;

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
    /// The bin slots of every heap, [`BIN_SLOTS`] for each, owned by whoever claims the heap.
    bin_slots: *mut *mut Block,
    /// How many NUMA nodes the region is split between, into how many zones the part of each node
    /// is split, and how many bitmap words there are. See [`Region::part`].
    nodes: usize,
    zones: usize,
    words: usize,
    /// How many heap slots may have been claimed. Slots at or beyond it are untouched, so the
    /// operations that visit every slot stop here.
    claimed_mark: AtomicUsize,
    clock: Clock,
    occupancy: Occupancy,
    #[cfg(feature = "metrics")]
    counters: crate::metrics::RegionCounters,
}

/// The state of the purge clock, on lines of its own. Every free of chunks writes `aging`, and
/// sharing a line with the fields above, which every claim of chunks reads, would make each free
/// take that line away from the cores that claim.
#[derive(Debug)]
struct Clock {
    _line: [CacheLine; 0],
    /// When the purge clock next ticks, in milliseconds on the monotonic clock.
    next_purge: AtomicU64,
    /// Whether any free chunk may still be waiting to be purged or to age towards it.
    aging: AtomicBool,
    /// How many purge passes and trims are holding free chunks right now, which makes the chunks
    /// look allocated to everyone else.
    purges_active: AtomicUsize,
    /// How many purge passes and trims have finished.
    purges_done: AtomicUsize,
    /// Set when the region keeps every page it has backed: from the start in a region backed up
    /// front, and from the first time the kernel refuses to take pages back, as it does for pages
    /// that the program locked in memory. The purge clock then stops, and `trim` leaves pages.
    keeps_pages: AtomicBool,
}

/// How many chunks slabs and blocks of whole chunks hold, which tells a big process from a small
/// one. Every claim and free of chunks writes it, so it keeps a line of its own, away from `aging`,
/// which the slow paths of allocation read.
#[derive(Debug)]
struct Occupancy {
    _line: [CacheLine; 0],
    chunks: AtomicUsize,
}

/// A region with this many chunks in use, 64 MiB, belongs to a process big enough that the
/// slabs of its classes that fill slabs go to the zone of transparent huge pages whatever the size
/// of their heap. See [`Region::is_big`].
const BIG_REGION_CHUNKS: usize = 1024;

// SAFETY: the raw pointer is an address into memory the region owns, and every shared structure
// behind the references is either atomic or handed out under an exclusive claim.
unsafe impl Send for Region<'_> {}
// SAFETY: as above.
unsafe impl Sync for Region<'_> {}

/// The storage a [`Region`] is assembled from.
///
/// * `base` must point to `chunk_count` chunks of writable memory aligned to 4 MiB, valid for as
///   long as `'a`, and used by nothing else.
/// * `words` must be zeroed bitmap storage for `chunk_count` chunks, `slabs` must hold
///   `chunk_count` unused descriptors, `map` `chunk_count` entries, and `heaps` unclaimed heap
///   slots.
/// * `bin_slots` must point to [`BIN_SLOTS`] writable pointers for each heap slot, valid for as
///   long as `'a` and used by nothing else.
pub(crate) struct Parts<'a> {
    pub(crate) base: *mut u8,
    pub(crate) words: &'a [AtomicU64],
    pub(crate) chunk_count: usize,
    pub(crate) slabs: &'a [Slab],
    pub(crate) map: &'a [AtomicU64],
    pub(crate) heaps: &'a [HeapData],
    pub(crate) bin_slots: *mut *mut Block,
    /// How many NUMA nodes to split the chunks between, at least 1.
    pub(crate) nodes: usize,
    /// Whether the part of each node has a zone for transparent huge pages. See [`Region::part`].
    pub(crate) huge_zone: bool,
}

impl<'a> Region<'a> {
    /// Assembles a region from its parts.
    ///
    /// # Safety
    ///
    /// The parts must satisfy the contract documented on [`Parts`].
    pub(crate) unsafe fn from_parts(parts: Parts<'a>) -> Self {
        debug_assert_eq!(parts.slabs.len(), parts.chunk_count);
        debug_assert_eq!(parts.map.len(), parts.chunk_count);
        // Heap ids must stay below the bit that marks huge blocks, and a huge block's length in
        // chunks must fit in the bits below it. See `Entry::huge`.
        debug_assert!(parts.heaps.len() <= Entry::HUGE_OWNER as usize);
        debug_assert!(parts.chunk_count < Entry::HUGE_OWNER as usize);
        let words = Chunks::words_for(parts.chunk_count);
        // A region too small to give every node a word of its own is not split at all, because
        // the parts are numbered by physical node. Zones are given up first, since they only
        // decide placement.
        let nodes = if parts.nodes <= words { parts.nodes.max(1) } else { 1 };
        let zones = if parts.huge_zone && 2 * nodes <= words { 2 } else { 1 };
        // Huge pages go back whole, and small pages one chunk at a time.
        let chunks = if zones > 1 {
            Chunks::with_granule(parts.words, parts.chunk_count, HUGE_PAGE_CHUNKS)
                .coarse_in_odd_parts(nodes * zones, HUGE_PURGE_WAIT)
        } else {
            Chunks::with_granule(parts.words, parts.chunk_count, 1)
        };
        Self {
            base: parts.base,
            chunks,
            slabs: parts.slabs,
            map: parts.map,
            heaps: parts.heaps,
            bin_slots: parts.bin_slots,
            nodes,
            zones,
            words,
            claimed_mark: AtomicUsize::new(0),
            clock: Clock {
                _line: [],
                next_purge: AtomicU64::new(0),
                aging: AtomicBool::new(false),
                purges_active: AtomicUsize::new(0),
                purges_done: AtomicUsize::new(0),
                keeps_pages: AtomicBool::new(false),
            },
            occupancy: Occupancy { _line: [], chunks: AtomicUsize::new(0) },
            #[cfg(feature = "metrics")]
            counters: crate::metrics::RegionCounters::new(),
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
                let entry = self.entry(self.chunk_index(ptr.as_ptr()));
                #[cfg(feature = "metrics")]
                self.counters.frees[entry.class().index()].add(1);
                let owner = entry.owner();
                // SAFETY: the caller gives the block up, and the chunk map names its owner.
                unsafe { self.heap_slot(owner).post_block(ptr.as_ptr().cast()) };
            }
            Some(Request::Huge { chunks, .. }) => {
                #[cfg(feature = "metrics")]
                {
                    self.counters.huge_frees.add(1);
                    self.counters.huge_bytes_freed.add((chunks << CHUNK_SHIFT) as u64);
                }
                self.free_chunks(self.chunk_index(ptr.as_ptr()), chunks);
                self.purge_if_due();
            }
            None => debug_assert!(false, "layout was never allocated by this region"),
        }
    }

    /// Returns the empty slabs of every idle heap to the region, and tells the operating system
    /// that free chunks no longer need physical pages. A region that keeps its pages, because it
    /// was backed up front with [`Commit::Populate`] or [`Commit::Lock`] or because the kernel
    /// refused to take pages back, only does the first.
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
        if self.clock.keeps_pages.load(Ordering::Relaxed) {
            return;
        }
        self.clock.purges_active.fetch_add(1, Ordering::Release);
        // The drain holds each run while its pages go back.
        self.chunks.drain(|run| self.give_back(run));
        self.clock.purges_done.fetch_add(1, Ordering::Release);
        self.clock.purges_active.fetch_sub(1, Ordering::Release);
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

    /// A snapshot of the allocator's metrics, summed over every heap slot that has been claimed.
    /// It costs a pass over those slots and the chunk bitmap, so call it now and then, not on a
    /// hot path. See [`Metrics`](crate::Metrics).
    #[cfg(feature = "metrics")]
    pub fn metrics(&self) -> crate::Metrics {
        use crate::metrics::Event;
        let claimed = self.claimed_mark.load(Ordering::Acquire);
        let heaps = &self.heaps[..claimed];
        let counters = &self.counters;
        let classes = crate::metrics::class_metrics(|class| {
            let unbound = counters.frees[class.index()].get();
            heaps
                .iter()
                .map(|heap| heap.class_counts(class))
                .fold((0, unbound, 0), |(allocs, frees, slabs), (a, f, s)| {
                    (allocs + a, frees + f, slabs.wrapping_add(s))
                })
        });
        let sum = |event| heaps.iter().map(|heap| heap.events().get(event)).sum::<u64>();
        let huge_allocated = sum(Event::HugeBytesAllocated);
        let huge_freed = sum(Event::HugeBytesFreed) + counters.huge_bytes_freed.get();
        let stats = self.stats();
        crate::Metrics {
            capacity: stats.capacity,
            chunk_bytes: stats.capacity - stats.free,
            heaps_claimed: claimed,
            heaps_in_use: stats.heaps_in_use,
            huge_live_bytes: huge_allocated.saturating_sub(huge_freed),
            classes,
            events: crate::metrics::Events {
                refills: sum(Event::Refills),
                slabs_added: sum(Event::SlabsAdded),
                slabs_released: sum(Event::SlabsReleased),
                blocks_sent: sum(Event::BlocksSent),
                messages_posted: sum(Event::MessagesPosted),
                mail_reads: sum(Event::MailReads),
                blocks_filed: sum(Event::BlocksFiled),
                huge_allocs: sum(Event::HugeAllocs),
                huge_frees: sum(Event::HugeFrees) + counters.huge_frees.get(),
                huge_cache_hits: sum(Event::HugeCacheHits),
                bins_grown: sum(Event::BinsGrown),
                bins_shrunk: sum(Event::BinsShrunk),
                zeroed_fresh: sum(Event::ZeroedFresh),
                blocks_spilled: sum(Event::BlocksSpilled),
                blocks_unspilled: sum(Event::BlocksUnspilled),
                purge_passes: counters.purge_passes.get(),
                chunks_purged: counters.chunks_purged.get(),
                chunks_reused: counters.chunks_reused.get(),
            },
        }
    }

    /// Whether `ptr` points into this region's managed memory.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let offset = ptr.addr().wrapping_sub(self.base.addr());
        offset < self.span()
    }

    /// The length of the managed memory in bytes.
    pub(crate) const fn span(&self) -> usize {
        self.chunks.len() << CHUNK_SHIFT
    }

    /// As [`alloc_chunks_zeroed`](Self::alloc_chunks_zeroed), without telling whether the chunks
    /// read as zeros.
    #[cfg(test)]
    pub(crate) fn alloc_chunks(
        &self,
        n: usize,
        align: usize,
        hint: usize,
        part: usize,
    ) -> Option<usize> {
        self.alloc_chunks_zeroed(n, align, hint, part).map(|(first, _)| first)
    }

    /// Claims `n` chunks as `Chunks`::alloc does, preferring part `part` (see [`Region::part`]),
    /// makes the pages of any that were given back to the OS ready for use, and tells whether the
    /// chunks read as zeros.
    pub(crate) fn alloc_chunks_zeroed(
        &self,
        n: usize,
        align: usize,
        hint: usize,
        part: usize,
    ) -> Option<(usize, bool)> {
        let home = self.home(part);
        let first = match self.chunks.alloc_near(n, align, hint, home.clone()) {
            Some(first) => first,
            None => self.alloc_chunks_after_purge(n, align, hint, home)?,
        };
        self.occupancy.chunks.fetch_add(n, Ordering::Relaxed);
        let zeroed = self.chunks.is_clean(first, n, sys::DISCARD_ZEROES);
        self.chunks.take_purged(first, n, |run| {
            #[cfg(feature = "metrics")]
            self.counters.chunks_reused.add(run.len() as u64);
            // SAFETY: the run is inside the region and belongs to us since the claim above.
            unsafe {
                sys::reuse(
                    NonNull::new_unchecked(self.chunk_ptr(run.start)),
                    run.len() << CHUNK_SHIFT,
                );
            }
        });
        Some((first, zeroed))
    }

    /// Whether so much of the region is in use that the process counts as big. Huge pages cost a
    /// small process memory it barely uses, since a thread's slabs of each class then back whole
    /// 2 MiB pages, and spare a big one most of its page faults, which many threads that fault
    /// at once pay for in time.
    ///
    /// The count only steers placement, so it may be approximate. A block handed to another thread
    /// through a relaxed store can be freed there before its claim's addition reaches the counter,
    /// which then wraps below zero for a moment; read as signed, that counts as small.
    pub(crate) fn is_big(&self) -> bool {
        self.occupancy.chunks.load(Ordering::Relaxed) as isize >= BIG_REGION_CHUNKS as isize
    }

    /// The part of the region that the chunks of NUMA node `node` come from, in its zone for
    /// transparent huge pages if `huge`.
    ///
    /// The part of each node is split in two. The upper zone is backed by huge pages and holds the
    /// chunks of heaps that have grown past a threshold, and of blocks of a huge page or more, so
    /// that a thread that uses little memory never holds a 2 MiB page it barely touches. Each zone
    /// is one range of addresses, so the advice is given once.
    pub(crate) const fn part(&self, node: usize, huge: bool) -> usize {
        (node % self.nodes) * self.zones + (huge && self.zones > 1) as usize
    }

    /// The bitmap words of part `part`. The parts of a node are next to each other.
    pub(crate) const fn home(&self, part: usize) -> Range<usize> {
        let parts = self.nodes * self.zones;
        let part = part % parts;
        part * self.words / parts..(part + 1) * self.words / parts
    }

    /// The bitmap words of all the parts of NUMA node `node`.
    pub(crate) const fn node_words(&self, node: usize) -> Range<usize> {
        let node = node % self.nodes;
        self.home(self.part(node, false)).start..self.home(self.part(node, true)).end
    }

    /// The NUMA node of the CPU the calling thread runs on, as the region numbers its parts.
    pub(crate) fn current_node(&self) -> usize {
        if self.nodes == 1 { 0 } else { sys::current_node() % self.nodes }
    }

    /// Extends the run of `had` chunks at `first` to `wants` chunks if the chunks after it are
    /// free, and returns whether it did.
    pub(crate) fn grow_in_place(&self, first: usize, had: usize, wants: usize) -> bool {
        let (start, more) = (first + had, wants - had);
        if start + more > self.chunks.len() {
            return false;
        }
        // A purge pass may hold the chunks for a moment, so a failed claim only counts when no
        // pass overlapped it, as for any other claim.
        loop {
            let active = self.clock.purges_active.load(Ordering::Acquire);
            let done = self.clock.purges_done.load(Ordering::Acquire);
            if self.chunks.claim_at(start, more) {
                break;
            }
            fence(Ordering::Acquire);
            if active == 0 &&
                self.clock.purges_active.load(Ordering::Acquire) == 0 &&
                self.clock.purges_done.load(Ordering::Acquire) == done
            {
                return false;
            }
            core::hint::spin_loop();
        }
        self.occupancy.chunks.fetch_add(more, Ordering::Relaxed);
        self.chunks.take_purged(start, more, |run| {
            #[cfg(feature = "metrics")]
            self.counters.chunks_reused.add(run.len() as u64);
            // SAFETY: the run is inside the region and belongs to us since the claim above.
            unsafe {
                sys::reuse(
                    NonNull::new_unchecked(self.chunk_ptr(run.start)),
                    run.len() << CHUNK_SHIFT,
                );
            }
        });
        true
    }

    /// Looks again for chunks after a failed claim, in case a purge pass was holding the free
    /// ones. The region is only reported full when no pass overlapped a failed attempt.
    #[cold]
    fn alloc_chunks_after_purge(
        &self,
        n: usize,
        align: usize,
        hint: usize,
        home: Range<usize>,
    ) -> Option<usize> {
        loop {
            let active = self.clock.purges_active.load(Ordering::Acquire);
            let done = self.clock.purges_done.load(Ordering::Acquire);
            // Every round looks at the bitmap, so chunks that are free for some other reason can
            // be used even while a pass that was descheduled is still holding its own.
            if let Some(first) = self.chunks.alloc_near(n, align, hint, home.clone()) {
                return Some(first);
            }
            // The attempt may have failed because of chunks that a pass claimed. Seeing such a
            // claim synchronises with the pass, so after this fence its counters are visible.
            fence(Ordering::Acquire);
            if active == 0 &&
                self.clock.purges_active.load(Ordering::Acquire) == 0 &&
                self.clock.purges_done.load(Ordering::Acquire) == done
            {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Frees chunks as `Chunks`::free_at does, and starts them ageing towards a purge unless the
    /// region keeps its pages.
    pub(crate) fn free_chunks(&self, first: usize, n: usize) {
        self.occupancy.chunks.fetch_sub(n, Ordering::Relaxed);
        self.chunks.free_at(first, n, sys::monotonic_ms());
        if self.clock.keeps_pages.load(Ordering::Relaxed) {
            return;
        }
        // A read-modify-write, so that the pass that consumes the flag also sees the bits above no
        // matter which free it read the flag from.
        self.clock.aging.fetch_or(true, Ordering::Release);
    }

    /// Gives the pages of a run of chunks that a purge pass or a trim holds back to the operating
    /// system, and returns whether they went. The first refusal, which comes from pages that are
    /// locked in memory, makes the region keep its pages from then on, since retrying would only
    /// be refused again.
    fn give_back(&self, run: Range<usize>) -> bool {
        if self.clock.keeps_pages.load(Ordering::Relaxed) {
            return false;
        }
        // SAFETY: the caller holds the run, so no allocation overlaps it.
        let given = unsafe {
            sys::discard(
                NonNull::new_unchecked(self.chunk_ptr(run.start)),
                run.len() << CHUNK_SHIFT,
            )
        };
        if !given {
            self.clock.keeps_pages.store(true, Ordering::Relaxed);
        }
        given
    }

    /// Runs the purge clock if free chunks are ageing and an interval has passed. Chunks that stay
    /// free for one to two intervals go back to the OS. This is called from paths that are already
    /// slow, so that an idle program pays nothing and a busy one pays a clock read now and then.
    #[inline]
    pub(crate) fn purge_if_due(&self) {
        if self.clock.aging.load(Ordering::Relaxed) {
            self.purge_when_due();
        }
    }

    #[cold]
    fn purge_when_due(&self) {
        if self.clock.keeps_pages.load(Ordering::Relaxed) {
            // Frees no longer set the flag, so this runs at most once after the pages are kept.
            self.clock.aging.store(false, Ordering::Relaxed);
            return;
        }
        let now = sys::monotonic_ms();
        let due = self.clock.next_purge.load(Ordering::Relaxed);
        let won = now >= due &&
            self.clock
                .next_purge
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
        self.clock.aging.swap(false, Ordering::Acquire);
        self.clock.purges_active.fetch_add(1, Ordering::Release);
        #[cfg(feature = "metrics")]
        self.counters.purge_passes.add(1);
        let pending = self.chunks.purge_pass(now, PURGE_INTERVAL_MS, |run| {
            #[cfg(feature = "metrics")]
            let len = run.len() as u64;
            let given = self.give_back(run);
            #[cfg(feature = "metrics")]
            self.counters.chunks_purged.add(if given { len } else { 0 });
            given
        });
        self.clock.purges_done.fetch_add(1, Ordering::Release);
        self.clock.purges_active.fetch_sub(1, Ordering::Release);
        if pending {
            self.clock.aging.fetch_or(true, Ordering::Release);
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

    /// Whether `ptr`, a block of a slab, is free in its slab's bitmap.
    ///
    /// # Safety
    ///
    /// No thread may be using the heap that owns the slab, because this reads its descriptor.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) unsafe fn is_free_block(&self, ptr: NonNull<u8>) -> bool {
        let class = self.entry(self.chunk_index(ptr.as_ptr())).class();
        // SAFETY: forwarded.
        unsafe { self.slab_of(ptr, class).owned() }.is_free(ptr.as_ptr())
    }

    /// The bin slots of heap `id`, with provenance over all of them.
    pub(crate) const fn bin_slots(&self, id: u32) -> *mut *mut Block {
        self.bin_slots.wrapping_add(id as usize * BIN_SLOTS)
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

    /// Records that the chunks from `first` hold a huge block of `chunks` chunks.
    pub(crate) fn set_huge(&self, first: usize, chunks: usize) {
        self.map[first].store(Entry::huge(chunks).0, Ordering::Relaxed);
    }

    /// The layout of the whole block that `ptr` points to, as the allocator holds it, or None if
    /// `ptr` is outside the region. It is only meaningful for the start of a live block.
    #[cfg(all(feature = "std", not(loom)))]
    pub(crate) fn block_layout(&self, ptr: *const u8) -> Option<Layout> {
        if !self.contains(ptr) {
            return None;
        }
        let entry = self.entry(self.chunk_index(ptr));
        let size = entry
            .huge_chunks()
            .map_or_else(|| entry.class().block_size(), |chunks| chunks << CHUNK_SHIFT);
        Layout::from_size_align(size, 16).ok()
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

/// What the chunk map records for a chunk: the heap that owns the slab and the slab's size class,
/// or for the first chunk of a huge block, how many chunks the block spans.
///
/// The owner sits in the low 32 bits, so the free path reads it with one load and a truncation.
/// The top bit of the owner marks a huge block, and no heap has an id with that bit. Frees through
/// the Rust interfaces know most huge blocks by their size, and C's `free` tells the two apart by
/// the entry. See [`Entry::huge`] for the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry(pub(crate) u64);

impl Entry {
    /// The entry for a slab of `class` owned by heap `owner`. Next to the class it keeps the byte
    /// offset of the class's bin in a heap, so that a free finds the bin without arithmetic.
    pub(crate) const fn new(owner: u32, class: Class) -> Self {
        let offset = (class.index() * crate::heap::BIN_SIZE) as u64;
        Self(owner as u64 | (class.index() as u64) << 32 | offset << 40)
    }

    /// The bit of the owner that marks the entry of the first chunk of a huge block. Heap ids stay
    /// below it.
    pub(crate) const HUGE_OWNER: u32 = 1 << 31;

    /// The entry for the first chunk of a huge block of `chunks` chunks.
    ///
    /// Read as a slab's, it is an entry of the smallest class, whose bin comes first, owned by
    /// heap `HUGE_OWNER | chunks`, which does not exist. A Rust free of a small size aligned past
    /// a chunk, which is a huge block, therefore needs no test of its own: the fast free takes the
    /// block for another heap's block of a class that goes home and calls `HeapData::send`, which
    /// tells it apart by the owner.
    pub(crate) const fn huge(chunks: usize) -> Self {
        Self((Self::HUGE_OWNER as usize | chunks) as u64)
    }

    /// The number of chunks of a huge block, or None if the entry is a slab's.
    pub(crate) const fn huge_chunks(self) -> Option<usize> {
        if self.is_huge() { Some((self.owner() & !Self::HUGE_OWNER) as usize) } else { None }
    }

    /// Whether the entry is that of the first chunk of a huge block.
    pub(crate) const fn is_huge(self) -> bool {
        self.owner() & Self::HUGE_OWNER != 0
    }

    /// The id of the heap that owns the slab.
    pub(crate) const fn owner(self) -> u32 {
        self.0 as u32
    }

    /// The byte offset of the bin of the slab's class in a heap.
    pub(crate) const fn bin_offset(self) -> usize {
        (self.0 >> 40) as usize
    }

    /// The size class of the slab.
    pub(crate) const fn class(self) -> Class {
        // SAFETY: entries are only ever written by [`Entry::new`] from a class.
        unsafe { Class::from_index_unchecked((self.0 >> 32) as u8 as usize) }
    }
}

/// The byte offsets of each part of a mapping.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    heaps: usize,
    bins: usize,
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
        let bins_at = heaps_at.checked_add(heaps.checked_mul(mem::size_of::<HeapData>())?)?;
        let bin_bytes = BIN_SLOTS * mem::size_of::<*mut Block>();
        let slabs_at = bins_at
            .checked_add(heaps.checked_mul(bin_bytes)?)?
            .checked_next_multiple_of(mem::align_of::<Slab>())?;
        let map_at = slabs_at.checked_add(chunks.checked_mul(mem::size_of::<Slab>())?)?;
        let words_at = map_at.checked_add(chunks.checked_mul(mem::size_of::<AtomicU64>())?)?;
        let words_end =
            words_at.checked_add(Chunks::storage_words(chunks) * mem::size_of::<AtomicU64>())?;
        let data = words_end.checked_next_multiple_of(SLAB_ALIGN)?;
        let total = data.checked_add(chunks.checked_mul(CHUNK_SIZE)?)?;
        Some(Self {
            heaps: heaps_at,
            bins: bins_at,
            slabs: slabs_at,
            map: map_at,
            words: words_at,
            data,
            total,
        })
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
            let nodes = if config.wants_numa() { sys::numa_nodes() } else { 1 };
            let region = Region::from_parts(Parts {
                base: at(geometry.data),
                words,
                chunk_count: chunks,
                slabs,
                map,
                heaps: heap_slots,
                bin_slots: at(geometry.bins).cast::<*mut Block>(),
                nodes,
                huge_zone: config.wants_huge_pages() && cfg!(target_os = "linux"),
            });
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
        let region = self.region();
        for node in 0..region.nodes {
            let chunks = |words: Range<usize>| {
                (words.start * 64).min(region.chunks.len())..
                    (words.end * 64).min(region.chunks.len())
            };
            let all = chunks(region.node_words(node));
            if region.nodes > 1 && !all.is_empty() {
                // SAFETY: the range is inside the data range.
                unsafe {
                    sys::prefer_node(
                        NonNull::new_unchecked(region.chunk_ptr(all.start)),
                        all.len() << CHUNK_SHIFT,
                        node,
                    );
                }
            }
            let huge = chunks(region.home(region.part(node, true)));
            if region.zones > 1 && !huge.is_empty() {
                // SAFETY: as above.
                unsafe {
                    sys::advise_huge(
                        NonNull::new_unchecked(region.chunk_ptr(huge.start)),
                        huge.len() << CHUNK_SHIFT,
                    );
                }
            }
        }
        match config.commit_mode() {
            Commit::Lazy => {}
            Commit::Populate | Commit::Lock => {
                // Memory backed up front stays backed, so the allocator never faults it in again.
                region.clock.keeps_pages.store(true, Ordering::Relaxed);
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
    fn every_numa_node_gets_a_part_of_the_region() {
        // Five words for four nodes, which an even split per word count would get wrong.
        let test = crate::testing::TestRegion::with_nodes(320, 1, 4);
        let region = test.region();
        let mut next = 0;
        for node in 0..4 {
            let home = region.home(node);
            assert_eq!(home.start, next, "the parts tile the bitmap");
            assert!(!home.is_empty(), "node {node} has no part");
            next = home.end;
        }
        assert_eq!(next, 5);
        let tiny = crate::testing::TestRegion::with_nodes(64, 1, 4);
        assert_eq!(tiny.region().home(3), 0..1, "a region smaller than the nodes is not split");
    }
    #[test]
    fn print_geometry_tmp() {
        let g = Geometry::new(1 << 20, 4096).unwrap();
        std::println!(
            "GEOM heaps {} bins {} slabs {} map {} words {} data {} heapdata {} slab {}",
            g.heaps >> 20,
            g.bins >> 20,
            g.slabs >> 20,
            g.map >> 20,
            g.words >> 20,
            g.data >> 20,
            mem::size_of::<HeapData>(),
            mem::size_of::<Slab>()
        );
    }

    #[test]
    fn every_node_gets_a_small_and_a_huge_page_zone_side_by_side() {
        let test = crate::testing::TestRegion::with_nodes(512, 1, 2).with_huge_zone();
        let region = test.region();
        let parts = [(0, false), (0, true), (1, false), (1, true)]
            .map(|(node, huge)| region.home(region.part(node, huge)));
        assert_eq!(parts, [0..2, 2..4, 4..6, 6..8], "the zones of a node are next to each other");
        assert_eq!(region.node_words(1), 4..8);
        let one = crate::testing::TestRegion::new(64, 1).with_huge_zone();
        let region = one.region();
        assert_eq!(region.part(0, true), region.part(0, false), "one word is not split");
        assert_eq!(region.node_words(0), 0..1);
    }

    #[test]
    fn growing_in_place_waits_out_a_purge_that_holds_the_next_chunks() {
        let test = crate::testing::TestRegion::new(20, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = |chunks: usize| Layout::from_size_align(chunks << CHUNK_SHIFT, 8).unwrap();
        let block = heap.alloc(layout(20)).unwrap();
        // SAFETY: the block is live with this layout.
        let block = unsafe { heap.realloc(block, layout(20), 9 << CHUNK_SHIFT) }.unwrap();
        // What a purge pass looks like while it holds the free tail.
        region.clock.purges_active.fetch_add(1, Ordering::Release);
        assert!(region.chunks.claim_at(9, 11));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(20));
                region.chunks.free_at(9, 11, 0);
                region.clock.purges_done.fetch_add(1, Ordering::Release);
                region.clock.purges_active.fetch_sub(1, Ordering::Release);
            });
            // SAFETY: the block is live with nine chunks.
            let grown = unsafe { heap.realloc(block, layout(9), 12 << CHUNK_SHIFT) };
            assert_eq!(grown, Some(block), "the pass let go of the chunks after the block");
        });
    }

    #[test]
    fn a_huge_blocks_entry_reads_as_a_block_of_no_heap_that_goes_home() {
        for chunks in [2, 64, Entry::HUGE_OWNER as usize - 1] {
            let entry = Entry::huge(chunks);
            assert_eq!(entry.huge_chunks(), Some(chunks));
            assert!(entry.owner() >= Entry::HUGE_OWNER, "no heap has the owner's id");
            assert_eq!((entry.class().index(), entry.bin_offset()), (0, 0), "the smallest class");
        }
        let slab = Entry::new(Entry::HUGE_OWNER - 1, Class::for_size(crate::MAX_SMALL));
        assert_eq!(slab.huge_chunks(), None, "the last heap's slabs are not huge");
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
        // The largest class lives in 512 KiB slabs, and its blocks are 64 KiB apart.
        let layout = Layout::from_size_align(64 << 10, 64 << 10).unwrap();
        let blocks: std::vec::Vec<_> = (0..4).map(|_| heap.alloc(layout).unwrap()).collect();
        assert!(blocks.iter().all(|block| block.as_ptr().addr() % (64 << 10) == 0));
    }

    #[test]
    fn a_retry_does_not_wait_for_a_purge_pass_while_other_chunks_are_free() {
        use crate::testing::TestRegion;
        use std::{sync::mpsc, time::Duration};

        let test = TestRegion::new(2, 1);
        let region = test.region();
        let a = region.alloc_chunks(1, 1, 0, 0).unwrap();
        region.alloc_chunks(1, 1, 0, 0).unwrap();
        // A pass that was descheduled while holding some other chunks.
        region.clock.purges_active.fetch_add(1, Ordering::Release);
        region.free_chunks(a, 1);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| sender.send(region.alloc_chunks_after_purge(1, 1, 0, 0..1)).unwrap());
            let found = receiver.recv_timeout(Duration::from_secs(5));
            // Let the retry finish whatever happened, so that the scope can end.
            region.clock.purges_active.fetch_sub(1, Ordering::Release);
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
                        let chunk = region.alloc_chunks(1, 1, 0, 0).expect("a chunk is free");
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

    /// Whether every page of the `len` bytes from `ptr` is resident.
    #[cfg(not(miri))]
    fn resident(ptr: *mut u8, len: usize) -> bool {
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mut pages = std::vec![0_u8; len.div_ceil(page)];
        // SAFETY: the range is mapped, and the vector has a byte for each of its pages.
        let status = unsafe { libc::mincore(ptr.cast(), len, pages.as_mut_ptr().cast()) };
        status == 0 && pages.iter().all(|&page| page & 1 == 1)
    }

    /// Writes old contents into a block of whole chunks, frees it, trims the region and asks for a
    /// zeroed block of the same size, which reuses the same chunks. `before_free` runs on the
    /// block just before it is freed. Returns the block's address and whether the zeroed one read
    /// as zeros.
    fn reuse_after_trim(
        region: &Region<'_>,
        before_free: impl FnOnce(NonNull<u8>),
    ) -> (NonNull<u8>, bool) {
        let heap = region.heap().unwrap();
        let layout = Layout::from_size_align(MIB, 16).unwrap();
        let block = heap.alloc(layout).unwrap();
        // SAFETY: the block holds a mebibyte.
        unsafe { block.as_ptr().write_bytes(0xAA, MIB) };
        before_free(block);
        // SAFETY: the block is live with this layout.
        unsafe { heap.dealloc(block, layout) };
        heap.flush();
        region.trim();
        let zeroed = heap.alloc_zeroed(layout).unwrap();
        assert_eq!(zeroed, block, "the zeroed block reuses the same chunks");
        // SAFETY: the block holds a mebibyte.
        let bytes = unsafe { slice::from_raw_parts(zeroed.as_ptr(), MIB) };
        (block, bytes.iter().all(|&byte| byte == 0))
    }

    #[test]
    #[cfg(not(miri))]
    fn memory_the_kernel_will_not_take_back_is_cleared_for_zeroed_requests() {
        let mapping = Mapping::new(&config(16 * MIB)).unwrap();
        let mut locked = true;
        // A program that pins its memory, as mlockall does, keeps the kernel from discarding it.
        let (_, zeroed) = reuse_after_trim(mapping.region(), |block| {
            // SAFETY: the block is inside the reservation.
            locked = unsafe { sys::lock(block, MIB) };
        });
        // Pinning memory needs a high enough RLIMIT_MEMLOCK, which CI cannot promise.
        if locked {
            assert!(zeroed, "a zeroed block kept the old contents of pinned pages");
        }
    }

    #[test]
    #[cfg(not(miri))]
    fn a_region_backed_up_front_keeps_its_pages() {
        for commit in [Commit::Populate, Commit::Lock] {
            let mapping = match Mapping::new(&config(16 * MIB).commit(commit)) {
                Ok(mapping) => mapping,
                Err(error) => {
                    assert_eq!((commit, error), (Commit::Lock, Error::LockFailed));
                    continue;
                }
            };
            let (block, zeroed) = reuse_after_trim(mapping.region(), |_| {});
            assert!(zeroed, "{commit:?}: a zeroed block kept old contents");
            assert!(resident(block.as_ptr(), MIB), "{commit:?}: the region gave pages back");
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
    use super::*;
    use crate::testing::TestRegion;
    use loom::thread;

    #[test]
    fn a_free_racing_a_purge_pass_never_strands_a_dirty_chunk() {
        loom::model(|| {
            // The threads share one region value, because the purge state lives in it.
            let test: &'static TestRegion =
                std::boxed::Box::leak(std::boxed::Box::new(TestRegion::new(1, 1)));
            let region: &'static Region<'static> =
                std::boxed::Box::leak(std::boxed::Box::new(test.region()));
            let chunk = region.alloc_chunks(1, 1, 0, 0).unwrap();
            let freer = thread::spawn(move || region.free_chunks(chunk, 1));
            let purger = thread::spawn(move || region.purge_pass(u64::MAX));
            freer.join().unwrap();
            purger.join().unwrap();
            // Whatever the pass missed has to be announced, or nothing would ever look again.
            let mut missed = 0;
            region.chunks.purge_pass(u64::MAX, 0, |run| {
                missed += run.len();
                true
            });
            assert!(
                missed == 0 || region.clock.aging.load(Ordering::Acquire),
                "a free chunk is dirty and nothing is going to purge it"
            );
        });
    }
}
