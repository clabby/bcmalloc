//! Helpers shared by the unit tests, the model-checking tests and the fuzz targets.
//!
//! This module only exists in test builds and behind the `testing` feature, so none of it is part
//! of the allocator.

use core::{alloc::Layout, cell::Cell, ptr, ptr::NonNull};
use std::{
    alloc::{alloc_zeroed, dealloc},
    boxed::Box,
    collections::BTreeMap,
    vec::Vec,
};

use crate::{
    chunks::{CHUNK_SHIFT, Chunks},
    heap::{Heap, HeapData},
    region::{Parts, Region},
    size_class::BIN_SLOTS,
    slab::{Block, Slab},
    sync::AtomicU64,
};

/// A small, fast, deterministic random number generator (xorshift64*).
///
/// Tests want reproducible pseudo-randomness without a dependency, and a failing seed is easy to
/// replay.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Creates a generator. Any seed works, including zero.
    pub const fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift, so mix it away.
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// Returns the next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Returns a value in `0..bound`. `bound` must be non-zero.
    pub const fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    /// Returns a value in `low..=high`.
    pub const fn between(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }
}

/// How many iterations a randomized test should run. Miri is orders of magnitude slower, so it gets
/// a token amount.
pub const fn iterations(native: usize) -> usize {
    if cfg!(miri) { native / 100 + 1 } else { native }
}

/// Zeroed bitmap storage for a [`Chunks`] allocator.
#[derive(Debug)]
pub(crate) struct Bitmap {
    words: Vec<AtomicU64>,
    len: usize,
    granule: usize,
}

impl Bitmap {
    /// Storage that tracks `len` chunks, all free.
    #[cfg(test)]
    pub(crate) fn new(len: usize) -> Self {
        Self::with_granule(len, 1)
    }

    /// Storage that tracks `len` chunks, all free, purged `granule` chunks at a time.
    pub(crate) fn with_granule(len: usize, granule: usize) -> Self {
        let words = (0..Chunks::storage_words(len)).map(|_| AtomicU64::new(0)).collect();
        Self { words, len, granule }
    }

    /// The allocator over this storage.
    pub(crate) fn chunks(&self) -> Chunks<'_> {
        Chunks::with_granule(&self.words, self.len, self.granule)
    }
}

/// Memory aligned like a slab, for tests that need real memory under a descriptor or a region.
#[derive(Debug)]
pub(crate) struct Buffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

/// The alignment of the largest slab, which is 64 chunks of 64 KiB.
pub(crate) const SLAB_ALIGN: usize = 4 << 20;

impl Buffer {
    /// `size` bytes aligned to [`SLAB_ALIGN`].
    pub(crate) fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, SLAB_ALIGN).expect("valid test layout");
        // SAFETY: the layout has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).expect("test allocation failed");
        Self { ptr, layout }
    }

    /// The start of the buffer.
    pub(crate) const fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

// SAFETY: a buffer is plain owned memory.
unsafe impl Send for Buffer {}
// SAFETY: sharing the buffer shares only its address.
unsafe impl Sync for Buffer {}

/// A [`Region`] built over ordinary memory instead of a mapping, so that tests can create small
/// regions cheaply and loom can model the atomics inside them.
#[derive(Debug)]
pub struct TestRegion {
    words: Box<[AtomicU64]>,
    slabs: Box<[Slab]>,
    map: Box<[AtomicU64]>,
    heaps: Box<[HeapData]>,
    bins: Box<[Cell<*mut Block>]>,
    memory: Buffer,
    chunks: usize,
    nodes: usize,
    huge_zone: bool,
}

impl TestRegion {
    /// A region of `chunks` chunks (64 KiB each) with `heaps` heap slots.
    ///
    /// # Panics
    ///
    /// Panics if `chunks` is zero.
    pub fn new(chunks: usize, heaps: usize) -> Self {
        assert!(chunks > 0, "a region needs at least one chunk");
        // SAFETY: an all-zero value is a valid free word, unused descriptor, empty map entry and
        // unclaimed heap, which is what the region relies on for its mapping as well.
        unsafe {
            Self {
                words: zeroed(Chunks::storage_words(chunks), || AtomicU64::new(0)),
                slabs: zeroed(chunks, Slab::new),
                map: zeroed(chunks, || AtomicU64::new(0)),
                heaps: zeroed(heaps, HeapData::new),
                bins: zeroed(heaps * BIN_SLOTS, || Cell::new(ptr::null_mut())),
                memory: Buffer::new(chunks << CHUNK_SHIFT),
                chunks,
                nodes: 1,
                huge_zone: false,
            }
        }
    }

    /// As [`TestRegion::new`], with the chunks split between `nodes` NUMA nodes as a machine with
    /// that many would split them.
    pub fn with_nodes(chunks: usize, heaps: usize, nodes: usize) -> Self {
        Self { nodes, ..Self::new(chunks, heaps) }
    }

    /// This region with the part of every node split into a zone for small pages and one for
    /// transparent huge pages, as on Linux.
    #[must_use]
    pub fn with_huge_zone(self) -> Self {
        Self { huge_zone: true, ..self }
    }

    /// The region over this storage.
    pub fn region(&self) -> Region<'_> {
        // SAFETY: the buffer is 4 MiB aligned and `chunks` chunks long, and everything else was
        // just created empty.
        unsafe {
            Region::from_parts(Parts {
                base: self.memory.as_ptr(),
                words: &self.words,
                chunk_count: self.chunks,
                slabs: &self.slabs,
                map: &self.map,
                heaps: &self.heaps,
                bin_slots: self.bins.as_ptr().cast::<*mut Block>().cast_mut(),
                nodes: self.nodes,
                huge_zone: self.huge_zone,
            })
        }
    }
}

/// `len` values that are all zero. Zeroing the memory in one go keeps Miri from interpreting the
/// construction of every field, which made building a region the slowest part of most tests. Loom
/// atomics are not plain memory, so under loom every value is built with `make`.
///
/// # Safety
///
/// An all-zero bit pattern must be a valid `T` that equals what `make` returns.
unsafe fn zeroed<T>(len: usize, make: impl Fn() -> T) -> Box<[T]> {
    if cfg!(loom) {
        return (0..len).map(|_| make()).collect();
    }
    // SAFETY: the caller vouches for the zero bit pattern.
    unsafe { Box::new_zeroed_slice(len).assume_init() }
}

/// A block the harness is tracking, and the byte it was filled with.
#[derive(Debug)]
struct Live {
    ptr: NonNull<u8>,
    layout: Layout,
    stamp: u8,
}

/// Drives several heaps of one region from a single thread and checks every promise the allocator
/// makes.
///
/// Using several heaps from one thread makes cross-heap frees deterministic, so a failing
/// sequence replays exactly. Every allocation is checked for alignment, for lying inside the
/// region and for overlapping no other live block. Every block is filled with a stamp that is
/// verified on free and realloc, which catches memory handed out twice.
#[derive(Debug)]
pub struct Harness<'r> {
    region: &'r Region<'r>,
    heaps: Vec<Heap<'r>>,
    live: Vec<Live>,
    spans: BTreeMap<usize, usize>,
    stamp: u8,
}

impl<'r> Harness<'r> {
    /// Claims `heaps` heaps of `region`.
    pub fn new(region: &'r Region<'r>, heaps: usize) -> Self {
        let heaps = (0..heaps).map(|_| region.heap().expect("enough heap slots")).collect();
        Self { region, heaps, live: Vec::new(), spans: BTreeMap::new(), stamp: 0 }
    }

    /// The number of live blocks.
    pub const fn live(&self) -> usize {
        self.live.len()
    }

    /// The heaps being driven.
    pub fn heaps(&self) -> &[Heap<'r>] {
        &self.heaps
    }

    /// Allocates through heap number `heap` (wrapping) and returns whether it succeeded.
    pub fn alloc(&mut self, heap: usize, size: usize, align: usize) -> bool {
        let layout = Layout::from_size_align(size, align).expect("valid layout");
        let heap = heap % self.heaps.len();
        let Some(ptr) = self.heaps[heap].alloc(layout) else { return false };
        assert_eq!(ptr.as_ptr().addr() % align, 0, "misaligned block for {layout:?}");
        assert!(self.region.contains(ptr.as_ptr()), "block outside the region");
        self.stamp = self.stamp.wrapping_add(1);
        self.track(ptr, size);
        // SAFETY: the block is ours and holds `size` bytes.
        unsafe { ptr.as_ptr().write_bytes(self.stamp, size) };
        self.live.push(Live { ptr, layout, stamp: self.stamp });
        true
    }

    /// Frees a live block, chosen by `victim` (wrapping), through heap `heap`. Returns false if
    /// nothing is live.
    pub fn free(&mut self, heap: usize, victim: usize) -> bool {
        if self.live.is_empty() {
            return false;
        }
        let block = self.live.swap_remove(victim % self.live.len());
        Self::verify(&block, block.layout.size());
        self.spans.remove(&block.ptr.as_ptr().addr());
        // SAFETY: the block is live, from this region, with this layout, and is dropped from the
        // live set so it cannot be freed twice.
        unsafe { self.heaps[heap % self.heaps.len()].dealloc(block.ptr, block.layout) };
        true
    }

    /// Reallocates a live block to `new_size` through heap `heap`. Returns false if nothing is
    /// live.
    pub fn realloc(&mut self, heap: usize, victim: usize, new_size: usize) -> bool {
        if self.live.is_empty() {
            return false;
        }
        let index = victim % self.live.len();
        let (ptr, old, stamp) = {
            let block = &self.live[index];
            (block.ptr, block.layout, block.stamp)
        };
        Self::verify(&self.live[index], old.size());
        let heap = heap % self.heaps.len();
        // SAFETY: the block is live with layout `old`, and the new size is non-zero.
        let Some(new) = (unsafe { self.heaps[heap].realloc(ptr, old, new_size) }) else {
            // A refused realloc must leave the block intact.
            Self::verify(&self.live[index], old.size());
            return false;
        };
        let kept = old.size().min(new_size);
        // SAFETY: the new block holds at least `kept` bytes.
        let prefix = unsafe { core::slice::from_raw_parts(new.as_ptr(), kept) };
        assert!(all_equal(prefix, stamp), "realloc lost the old contents");
        assert_eq!(new.as_ptr().addr() % old.align(), 0, "realloc broke alignment");
        self.spans.remove(&ptr.as_ptr().addr());
        self.track(new, new_size);
        // SAFETY: the new block holds `new_size` bytes and is ours.
        unsafe { new.as_ptr().write_bytes(stamp, new_size) };
        let layout = Layout::from_size_align(new_size, old.align()).expect("valid layout");
        self.live[index] = Live { ptr: new, layout, stamp };
        true
    }

    /// Checks the structural invariants of every heap, and that no block is held in two places
    /// at once or is held anywhere while it is live.
    pub fn validate(&self) {
        let mut seen: std::collections::BTreeSet<usize> =
            self.live.iter().map(|block| block.ptr.as_ptr().addr()).collect();
        for heap in &self.heaps {
            heap.validate_into(&mut seen);
        }
        // Huge blocks are runs of chunks and not blocks of a slab.
        for block in &self.live {
            if block.layout.size() > crate::MAX_SMALL {
                seen.remove(&block.ptr.as_ptr().addr());
            }
        }
        for &addr in &seen {
            let block = NonNull::new(self.region.chunk_ptr(0).with_addr(addr)).expect("a block");
            // SAFETY: the harness drives every heap from this thread, and none is in use now.
            let free = unsafe { self.region.is_free_block(block) };
            assert!(!free, "a block that is live or held is also free in its slab");
        }
    }

    /// Releases heap number `heap` (wrapping) and claims a slot again, which is the same one. The
    /// new handle inherits the slabs and hot lists that the old one left behind.
    pub fn recycle(&mut self, heap: usize) {
        let slot = heap % self.heaps.len();
        drop(self.heaps.remove(slot));
        self.heaps.insert(slot, self.region.heap().expect("the released slot is free"));
    }

    /// Flushes heap number `heap` (wrapping).
    pub fn flush(&self, heap: usize) {
        self.heaps[heap % self.heaps.len()].flush();
    }

    /// Frees everything, flushes every heap and checks that the region is whole again, which proves
    /// that nothing leaked and that all free memory coalesced.
    pub fn finish(mut self) {
        self.validate();
        let mut round = 0;
        while self.free(round, 0) {
            round += 1;
        }
        self.validate();
        // The first round sends what each heap holds for the others, and the next ones let every
        // owner take it in and send back the messages it was carried in.
        for _ in 0..3 {
            for heap in &self.heaps {
                heap.flush();
            }
        }
        self.validate();
        let stats = self.region.stats();
        assert_eq!(stats.free, stats.capacity, "memory leaked or failed to coalesce");
    }

    /// Records a new block, checking it overlaps no neighbour.
    fn track(&mut self, ptr: NonNull<u8>, size: usize) {
        let (start, end) = (ptr.as_ptr().addr(), ptr.as_ptr().addr() + size.max(1));
        if let Some((_, &previous_end)) = self.spans.range(..=start).next_back() {
            assert!(previous_end <= start, "block at {start:#x} overlaps the one before it");
        }
        if let Some((&next_start, _)) = self.spans.range(start..).next() {
            assert!(next_start >= end, "block at {start:#x} overlaps the one after it");
        }
        self.spans.insert(start, end);
    }

    fn verify(block: &Live, len: usize) {
        // SAFETY: the block is live and holds at least `len` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(block.ptr.as_ptr(), len) };
        assert!(all_equal(bytes, block.stamp), "block contents were overwritten");
    }
}

/// Whether every byte of `bytes` equals `value`.
///
/// Miri interprets every step, so a byte-by-byte scan of a multi-megabyte block would take minutes.
/// Under Miri this checks both ends and a stride through the middle instead. Two overlapping blocks
/// carry different stamps at their edges, so overlap is still caught.
fn all_equal(bytes: &[u8], value: u8) -> bool {
    if !cfg!(miri) {
        return bytes.iter().all(|&byte| byte == value);
    }
    let edge = bytes.len().min(64);
    bytes[..edge]
        .iter()
        .chain(&bytes[bytes.len() - edge..])
        .chain(bytes.iter().step_by(4099))
        .all(|&byte| byte == value)
}

/// Picks a request size, weighted towards small blocks but reaching every size class, slab size
/// and huge allocations.
const fn random_size(rng: &mut Rng) -> usize {
    match rng.below(10) {
        0..=5 => rng.between(1, 256),
        6..=7 => rng.between(1, 8 << 10),
        8 => rng.between(1, 600 << 10),
        _ => rng.between(1, 6 << 20),
    }
}

/// Runs `steps` random operations from `seed` over three heaps of `region`, then checks the
/// region is whole again.
pub fn random_traffic(region: &Region<'_>, seed: u64, steps: usize) {
    let mut rng = Rng::new(seed);
    let mut harness = Harness::new(region, 3);
    for step in 0..steps {
        let heap = rng.below(3);
        match rng.below(21) {
            0..=9 => {
                let widest = if rng.below(8) == 0 { 17 } else { 5 };
                let align = 1 << rng.below(widest);
                harness.alloc(heap, random_size(&mut rng), align);
            }
            10..=16 => {
                harness.free(heap, rng.below(1 << 20));
            }
            17..=18 => {
                harness.realloc(heap, rng.below(1 << 20), random_size(&mut rng));
            }
            19 => harness.flush(heap),
            _ => harness.recycle(heap),
        }
        // The check walks every list, which Miri does slowly, so it samples there.
        if step % if cfg!(miri) { 32 } else { 8 } == 0 {
            harness.validate();
        }
    }
    harness.finish();
}

/// Interprets `data` as a sequence of allocator operations and checks them with a [`Harness`].
///
/// This is the body of the fuzz target. Every byte string is a valid program, so a fuzzer can
/// mutate it freely. An operation cut short by the end of the input is dropped.
pub fn run_ops(data: &[u8]) {
    let test = TestRegion::new(160, 4);
    let region = test.region();
    let mut harness = Harness::new(&region, 3);
    let _ = interpret(&mut harness, &mut data.iter().map(|&byte| usize::from(byte)));
    harness.finish();
}

/// Runs operations read from `bytes` until they run out.
fn interpret(harness: &mut Harness<'_>, bytes: &mut impl Iterator<Item = usize>) -> Option<()> {
    for step in 0_usize.. {
        let (op, heap) = (bytes.next()?, bytes.next()?);
        if step % 4 == 0 {
            harness.validate();
        }
        match op % 16 {
            0..=7 => {
                let limit = [256, 4 << 10, 600 << 10, 6 << 20][bytes.next()? % 4];
                let size = (bytes.next()? | bytes.next()? << 8 | bytes.next()? << 16) % limit + 1;
                harness.alloc(heap, size, 1 << (bytes.next()? % 18));
            }
            8..=11 => {
                harness.free(heap, bytes.next()? | bytes.next()? << 8);
            }
            12..=13 => {
                let size =
                    (bytes.next()? | bytes.next()? << 8 | bytes.next()? << 16) % (1 << 20) + 1;
                harness.realloc(heap, bytes.next()? | bytes.next()? << 8, size);
            }
            14 => harness.flush(heap),
            _ => harness.recycle(heap),
        }
    }
    None
}

/// A plain reference implementation of the chunk allocator's contract, checked against the real
/// one after every operation.
///
/// It records which chunks are taken and asserts that every run the allocator hands out is free,
/// aligned and in bounds, that every refusal is justified, and that the free count agrees.
#[derive(Debug)]
pub(crate) struct ChunkModel {
    bitmap: Bitmap,
    taken: Vec<bool>,
    live: Vec<(usize, usize)>,
    /// Whether the chunk has ever been handed out, which is what makes it worth purging.
    touched: Vec<bool>,
    /// Whether a purge pass has given the chunk's pages back and nobody has claimed it since.
    purged: Vec<bool>,
    /// The clock of the model, in milliseconds, advanced by the purge passes.
    now: u64,
    /// The latest time that a chunk of each 64-chunk word was freed.
    stamps: Vec<u64>,
    /// How many chunks the purge takes together.
    granule: usize,
}

impl ChunkModel {
    /// A model of `len` free chunks that are purged `granule` at a time.
    pub(crate) fn with_granule(len: usize, granule: usize) -> Self {
        Self {
            bitmap: Bitmap::with_granule(len, granule),
            taken: std::vec![false; len],
            live: Vec::new(),
            touched: std::vec![false; len],
            purged: std::vec![false; len],
            now: 0,
            stamps: std::vec![0; len.div_ceil(64)],
            granule,
        }
    }

    /// How many runs are live.
    #[cfg(all(test, not(loom)))]
    pub(crate) const fn live(&self) -> usize {
        self.live.len()
    }

    /// Allocates a run and checks the result. Returns whether it succeeded.
    pub(crate) fn alloc(&mut self, n: usize, align: usize, hint: usize) -> bool {
        let found = self.bitmap.chunks().alloc(n, align, hint);
        let succeeded = found.is_some();
        match found {
            Some(first) => {
                assert_eq!(first % align, 0, "run is not aligned");
                assert!(first + n <= self.taken.len(), "run is out of bounds");
                assert!(self.taken[first..first + n].iter().all(|taken| !taken), "runs overlap");
                self.taken[first..first + n].fill(true);
                self.touched[first..first + n].fill(true);
                self.live.push((first, n));
                // Every purged chunk of the run must be reported, once.
                let mut reported = Vec::new();
                self.bitmap.chunks().take_purged(first, n, |run| reported.extend(run));
                let expected: Vec<usize> =
                    (first..first + n).filter(|&chunk| self.purged[chunk]).collect();
                assert_eq!(reported, expected, "the purged chunks of the run were misreported");
                self.purged[first..first + n].fill(false);
            }
            None => assert!(!self.has_room(n, align), "refused {n} chunks although they fit"),
        }
        self.check();
        succeeded
    }

    /// Frees a live run chosen by `victim` (wrapping). Returns false if nothing is live.
    pub(crate) fn free(&mut self, victim: usize) -> bool {
        if self.live.is_empty() {
            return false;
        }
        let (first, n) = self.live.swap_remove(victim % self.live.len());
        self.now += 1;
        self.bitmap.chunks().free_at(first, n, self.now);
        self.taken[first..first + n].fill(false);
        for word in first / 64..=(first + n - 1) / 64 {
            self.stamps[word] = self.now;
        }
        self.check();
        true
    }

    /// Runs one purge pass after advancing the clock by `elapsed` milliseconds, and checks that it
    /// takes exactly the wholly free granules that hold a dirty chunk in words that have been quiet
    /// for the delay, and never a chunk that is in use. Returns how many chunks it took.
    pub(crate) fn purge(&mut self, elapsed: u64) -> usize {
        const DELAY: u64 = 5;
        self.now += elapsed;
        let mut visited = Vec::new();
        let pending = self.bitmap.chunks().purge_pass(self.now, DELAY, |run| visited.extend(run));
        let len = self.taken.len();
        let groups: Vec<_> = (0..len)
            .step_by(self.granule)
            .map(|start| start..(start + self.granule).min(len))
            .filter(|group| {
                group.clone().all(|chunk| !self.taken[chunk]) &&
                    group.clone().any(|chunk| self.touched[chunk] && !self.purged[chunk])
            })
            .collect();
        let mut expected = 0;
        let mut waiting = false;
        let mut taken = std::vec![false; len];
        for group in groups {
            if self.now.saturating_sub(self.stamps[group.start / 64]) >= DELAY {
                for chunk in group {
                    taken[chunk] = true;
                    self.purged[chunk] = true;
                    expected += 1;
                }
            } else {
                waiting = true;
            }
        }
        for (chunk, &purge) in taken.iter().enumerate() {
            assert_eq!(
                visited.contains(&chunk),
                purge,
                "chunk {chunk} was purged at the wrong time"
            );
        }
        assert_eq!(pending, waiting, "the pass misreports whether chunks are still waiting");
        self.check();
        expected
    }

    /// Whether a run of `n` chunks could be placed. Runs of up to 64 chunks stay inside one
    /// bitmap word and longer runs start on a word boundary.
    fn has_room(&self, n: usize, align: usize) -> bool {
        let step = if n > 64 { 64 } else { align };
        (0..(self.taken.len() + 1).saturating_sub(n))
            .step_by(step)
            .filter(|start| n > 64 || start / 64 == (start + n - 1) / 64)
            .any(|start| self.taken[start..start + n].iter().all(|taken| !taken))
    }

    fn check(&self) {
        let free = self.taken.iter().filter(|taken| !**taken).count();
        assert_eq!(self.bitmap.chunks().free_chunks(), free, "free count disagrees with the model");
    }
}

/// Interprets `data` as a sequence of chunk allocator operations and checks them with a
/// a `ChunkModel`. This is the body of the chunk fuzz target.
pub fn run_chunk_ops(data: &[u8]) {
    let mut bytes = data.iter().map(|&byte| usize::from(byte));
    let Some(len) = bytes.next() else { return };
    // The top bit of the first byte selects huge page granules of 32 chunks.
    let granule = if len & 0x80 != 0 { 32 } else { 1 };
    let mut model = ChunkModel::with_granule(1 + len % 400, granule);
    let _ = interpret_chunks(&mut model, &mut bytes);
}

/// Runs chunk operations read from `bytes` until they run out.
fn interpret_chunks(model: &mut ChunkModel, bytes: &mut impl Iterator<Item = usize>) -> Option<()> {
    loop {
        let op = bytes.next()? % 7;
        if op == 6 {
            let elapsed = bytes.next()? % 12;
            model.purge(elapsed as u64);
        } else if op < 4 {
            let n = match bytes.next()? % 4 {
                0 => 1,
                1 => 1 + bytes.next()? % 20,
                2 => 64,
                _ => 1 + bytes.next()? % 200,
            };
            model.alloc(n, 1 << (bytes.next()? % 7), bytes.next()? % 8);
        } else {
            model.free(bytes.next()?);
        }
    }
}
