//! A lock-free allocator for runs of contiguous 64 KiB chunks.
//!
//! The region the allocator manages is divided into chunks, and every chunk is one bit in a bitmap.
//! A one means the chunk is allocated and a zero means it is free. Allocating a run finds zero bits
//! and turns them into ones with a single compare-and-swap. Freeing is a single atomic clear. There
//! are no free lists, no headers and no per-chunk metadata, so nothing here can be corrupted by a
//! stale pointer, and neighbouring free chunks are coalesced simply by being neighbours.
//!
//! ```text
//!             chunks 0 to 63       chunks 64 to 127     chunks 128 to 191
//!           +--------------------+--------------------+--------------------+
//!   bitmap  | 1110010000 ... 00  | 0000000011 ... 00  | 0000000000 ... 00  | ...
//!           +--------------------+--------------------+--------------------+
//!             word 0               word 1               word 2
//! ```
//!
//! Bits past the last chunk are never handed out.
//!
//! # How requests are served
//!
//! A run of up to 64 chunks fits in one word, so claiming it takes one compare-and-swap. A swap
//! only fails because another thread's swap succeeded, which makes this path lock-free. A run may
//! be required to start at a multiple of an alignment. That is how slabs of 8 and 64 chunks stay
//! aligned to their own size.
//!
//! A run of more than 64 chunks spans several words, and no single atomic covers that. The
//! allocator claims the words in order and rolls back if it loses a race. This path is
//! obstruction-free instead of lock-free, which is acceptable because it only serves huge
//! allocations.
//!
//! Freeing is one `fetch_and` for each word the run touches.
//!
//! # Where runs are placed
//!
//! Single chunks come from the low end of the region, starting at a hint so that threads spread
//! over different words. Longer runs come from the high end. Keeping the two apart stops long-lived
//! small slabs from scattering through the address space and breaking up the room that large runs
//! need.
//!
//! ```text
//!   low addresses                                                      high addresses
//!   +-----------------------------------------------------------------------------+
//!   | ## # #### ................... free ................... ######## ############## |
//!   +-----------------------------------------------------------------------------+
//!     single chunks grow up -->                        <-- runs of 8, 64, ... grow down
//! ```
//!
//! # Memory ordering
//!
//! Claims use `Acquire` and frees use `Release`. A block is usually written by one thread, freed,
//! and then reused by another. The release on the free and the acquire on the next claim make the
//! first thread's writes visible to the second.

use core::ops::Range;

use crate::sync::{AtomicU64, Ordering};

/// log2 of the chunk size.
pub(crate) const CHUNK_SHIFT: usize = 16;

/// The granule of address space the allocator hands out, which is 64 KiB.
pub(crate) const CHUNK_SIZE: usize = 1 << CHUNK_SHIFT;

/// Chunks per bitmap word.
const WORD: usize = 64;

/// The bitmap of chunks. See the [module documentation](self).
#[derive(Debug)]
pub(crate) struct Chunks<'a> {
    words: &'a [AtomicU64],
    len: usize,
}

impl<'a> Chunks<'a> {
    /// The number of bitmap words needed to track `chunks` chunks.
    pub(crate) const fn words_for(chunks: usize) -> usize {
        chunks.div_ceil(WORD)
    }

    /// Wraps zeroed bitmap storage that tracks `len` chunks.
    ///
    /// # Panics
    ///
    /// Panics if `words` is too short for `len`.
    pub(crate) fn new(words: &'a [AtomicU64], len: usize) -> Self {
        assert!(words.len() >= Self::words_for(len), "bitmap too short for {len} chunks");
        Self { words, len }
    }

    /// The number of chunks tracked.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// The number of chunks currently free.
    pub(crate) fn free_chunks(&self) -> usize {
        let allocated: usize = self
            .words
            .iter()
            .take(Self::words_for(self.len))
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum();
        self.len - allocated
    }

    /// Claims `n` contiguous chunks and returns the index of the first.
    ///
    /// The run starts at a multiple of `align`, which must be a power of two and at most 64.
    /// `hint` is the bitmap word at which single-chunk searches begin. Returns `None` if no run
    /// is free.
    pub(crate) fn alloc(&self, n: usize, align: usize, hint: usize) -> Option<usize> {
        debug_assert!(n > 0 && align.is_power_of_two() && align <= WORD);
        let words = Self::words_for(self.len);
        match n {
            _ if n > self.len => None,
            1 => {
                (0..words).map(|i| (hint + i) % words).find_map(|w| self.claim(w, 1, align, false))
            }
            2..=WORD => (0..words).rev().find_map(|w| self.claim(w, n, align, true)),
            _ => self.claim_span(n),
        }
    }

    /// Returns `n` chunks starting at `first`.
    ///
    /// The run must be exactly one previously returned by [`alloc`](Self::alloc), or a part of
    /// one.
    pub(crate) fn free(&self, first: usize, n: usize) {
        debug_assert!(n > 0 && first + n <= self.len);
        let mut chunk = first;
        let end = first + n;
        while chunk < end {
            let (word, offset) = (chunk / WORD, chunk % WORD);
            let take = (WORD - offset).min(end - chunk);
            let mask = run_mask(take, offset);
            let before = self.words[word].fetch_and(!mask, Ordering::Release);
            debug_assert_eq!(before & mask, mask, "freeing chunks that are not allocated");
            chunk += take;
        }
    }

    /// Claims every free chunk one word at a time, hands each maximal free run to `visit`, and
    /// gives the chunks back.
    ///
    /// This is how the allocator returns physical memory to the OS. While `visit` runs the run
    /// belongs to the caller, so nobody can allocate and use it at the same time. The cost is that
    /// other threads see up to 64 chunks of free memory as missing while a word is being drained.
    pub(crate) fn drain(&self, mut visit: impl FnMut(Range<usize>)) {
        for (w, word) in self.words.iter().take(Self::words_for(self.len)).enumerate() {
            let valid = self.valid_mask(w);
            let claimed = !word.fetch_or(valid, Ordering::Acquire) & valid;
            let mut rest = claimed;
            while rest != 0 {
                let start = rest.trailing_zeros() as usize;
                let run = (rest >> start).trailing_ones() as usize;
                visit(w * WORD + start..w * WORD + start + run);
                rest &= !run_mask(run, start);
            }
            if claimed != 0 {
                word.fetch_and(!claimed, Ordering::Release);
            }
        }
    }

    /// Bits of word `w` that correspond to real chunks.
    const fn valid_mask(&self, w: usize) -> u64 {
        let remaining = self.len - w * WORD;
        if remaining >= WORD { u64::MAX } else { (1 << remaining) - 1 }
    }

    /// Claims a run of `n <= 64` chunks inside word `w`, preferring the highest start if `highest`
    /// and the lowest otherwise.
    fn claim(&self, w: usize, n: usize, align: usize, highest: bool) -> Option<usize> {
        let word = &self.words[w];
        let unusable = !self.valid_mask(w);
        let mut seen = word.load(Ordering::Relaxed);
        loop {
            let starts = run_starts(!(seen | unusable), n, align);
            if starts == 0 {
                return None;
            }
            let offset =
                if highest { 63 - starts.leading_zeros() } else { starts.trailing_zeros() };
            let offset = offset as usize;
            match word.compare_exchange_weak(
                seen,
                seen | run_mask(n, offset),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(w * WORD + offset),
                Err(actual) => seen = actual,
            }
        }
    }

    /// Claims `n > 64` chunks that start on a word boundary, preferring the highest address.
    fn claim_span(&self, n: usize) -> Option<usize> {
        let (full, rest) = (n / WORD, n % WORD);
        (0..=(self.len - n) / WORD)
            .rev()
            .find(|&first| {
                self.looks_free(first, full, rest) && self.try_claim_span(first, full, rest)
            })
            .map(|first| first * WORD)
    }

    /// A cheap, racy pre-check that avoids claiming words that are obviously taken.
    fn looks_free(&self, first: usize, full: usize, rest: usize) -> bool {
        self.words[first..first + full].iter().all(|w| w.load(Ordering::Relaxed) == 0) &&
            (rest == 0 ||
                self.words[first + full].load(Ordering::Relaxed) & run_mask(rest, 0) == 0)
    }

    /// Claims `full` whole words and the low `rest` bits of the next, undoing everything if any
    /// of them is taken.
    fn try_claim_span(&self, first: usize, full: usize, rest: usize) -> bool {
        for (claimed, word) in self.words[first..first + full].iter().enumerate() {
            if word.compare_exchange(0, u64::MAX, Ordering::Acquire, Ordering::Relaxed).is_err() {
                self.undo(first, claimed);
                return false;
            }
        }
        if rest > 0 {
            let (word, mask) = (&self.words[first + full], run_mask(rest, 0));
            let mut seen = word.load(Ordering::Relaxed);
            loop {
                if seen & mask != 0 {
                    self.undo(first, full);
                    return false;
                }
                match word.compare_exchange_weak(
                    seen,
                    seen | mask,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => seen = actual,
                }
            }
        }
        true
    }

    /// Releases the first `claimed` whole words of a span claim that is being abandoned. The words
    /// are entirely ours, so a plain store is enough.
    fn undo(&self, first: usize, claimed: usize) {
        for word in &self.words[first..first + claimed] {
            word.store(0, Ordering::Release);
        }
    }
}

/// A mask of `n` one bits starting at bit `offset`.
const fn run_mask(n: usize, offset: usize) -> u64 {
    if n == WORD { u64::MAX } else { ((1 << n) - 1) << offset }
}

/// The set of positions where a run of `n` free bits begins at a multiple of `align`.
///
/// `free` has a one wherever a chunk is free. Folding the word onto itself leaves a one only where
/// `n` ones begin. Doubling the folded span each round takes just `log2(n)` steps.
const fn run_starts(free: u64, n: usize, align: usize) -> u64 {
    let mut starts = free;
    let mut span = 1;
    while span < n {
        let step = if span < n - span { span } else { n - span };
        starts &= starts >> step;
        span += step;
    }
    // One bit at every multiple of `align`, which is u64::MAX / (2^align - 1).
    starts & (u64::MAX / (u64::MAX >> (WORD - align)))
}

#[cfg(all(test, not(loom)))]
mod test {
    use std::{vec, vec::Vec};

    use super::*;
    use crate::testing::{Bitmap, ChunkModel, Rng, iterations};

    #[test]
    fn single_chunks_fill_the_bitmap_from_the_bottom() {
        // Two full words and a two-bit tail that must stay off limits.
        let bitmap = Bitmap::new(130);
        let chunks = bitmap.chunks();
        let handed_out: Vec<_> = (0..130).map(|_| chunks.alloc(1, 1, 0).unwrap()).collect();
        assert_eq!(handed_out, (0..130).collect::<Vec<_>>());
        assert_eq!(chunks.alloc(1, 1, 0), None);
        assert_eq!(chunks.free_chunks(), 0);
    }

    #[test]
    fn single_chunk_searches_start_at_the_hint_and_wrap() {
        let bitmap = Bitmap::new(256);
        let chunks = bitmap.chunks();
        assert_eq!(chunks.alloc(1, 1, 2), Some(128));
        assert_eq!(chunks.alloc(1, 1, 3), Some(192));
        // A hint past the end of the bitmap wraps around instead of failing.
        assert_eq!(chunks.alloc(1, 1, 7), Some(192 + 1));
    }

    #[test]
    fn runs_come_from_the_top_and_are_aligned() {
        let bitmap = Bitmap::new(256);
        let chunks = bitmap.chunks();
        assert_eq!(chunks.alloc(1, 1, 0), Some(0));
        assert_eq!(chunks.alloc(8, 8, 0), Some(248));
        assert_eq!(chunks.alloc(64, 64, 0), Some(128));
        assert_eq!(chunks.alloc(5, 1, 0), Some(243));
        for align in [1, 2, 4, 8, 16, 32, 64] {
            if let Some(first) = chunks.alloc(2, align, 0) {
                assert_eq!(first % align, 0);
            }
        }
    }

    #[test]
    fn single_chunks_honour_alignment_too() {
        // Found by the chunk fuzz target. A one-chunk request used to ignore its alignment.
        let bitmap = Bitmap::new(2);
        let chunks = bitmap.chunks();
        assert_eq!(chunks.alloc(1, 1, 0), Some(0));
        assert_eq!(chunks.alloc(1, 8, 0), None, "chunk 1 is not a multiple of 8");
        assert_eq!(chunks.free_chunks(), 1);
    }

    #[test]
    fn freed_runs_coalesce() {
        let bitmap = Bitmap::new(256);
        let chunks = bitmap.chunks();
        let mut singles: Vec<_> = (0..256).map(|_| chunks.alloc(1, 1, 0).unwrap()).collect();
        assert_eq!(chunks.alloc(2, 1, 0), None);
        // Free in a scattered order, since adjacency and not order is what makes room.
        singles.sort_by_key(|chunk| chunk.wrapping_mul(0x9E37));
        for chunk in singles {
            chunks.free(chunk, 1);
        }
        assert_eq!(chunks.free_chunks(), 256);
        assert_eq!(chunks.alloc(256, 1, 0), Some(0));
    }

    #[test]
    fn spans_cover_several_words_and_prefer_the_top() {
        let bitmap = Bitmap::new(512);
        let chunks = bitmap.chunks();
        // 130 chunks = two whole words and a two-bit remainder.
        assert_eq!(chunks.alloc(130, 1, 0), Some(5 * 64));
        assert_eq!(chunks.alloc(130, 1, 0), Some(2 * 64));
        assert_eq!(chunks.free_chunks(), 512 - 260);
        chunks.free(5 * 64, 130);
        chunks.free(2 * 64, 130);
        assert_eq!(chunks.free_chunks(), 512);
    }

    #[test]
    fn failed_spans_leave_no_trace() {
        let bitmap = Bitmap::new(256);
        let chunks = bitmap.chunks();
        // One chunk taken in every word leaves plenty of free space but no run longer than 63.
        let pinned: Vec<_> = (0..4).map(|w| chunks.alloc(1, 1, w).unwrap()).collect();
        assert_eq!(pinned, [0, 64, 128, 192]);
        let free_before = chunks.free_chunks();
        assert_eq!(chunks.alloc(65, 1, 0), None);
        assert_eq!(chunks.alloc(200, 1, 0), None);
        assert_eq!(chunks.free_chunks(), free_before);
    }

    #[test]
    fn requests_larger_than_the_region_fail() {
        let bitmap = Bitmap::new(100);
        assert_eq!(bitmap.chunks().alloc(101, 1, 0), None);
        assert_eq!(bitmap.chunks().alloc(100, 1, 0), Some(0));
    }

    #[test]
    fn drain_visits_exactly_the_free_runs_and_restores_them() {
        let bitmap = Bitmap::new(200);
        let chunks = bitmap.chunks();
        let a = chunks.alloc(10, 1, 0).unwrap();
        let b = chunks.alloc(70, 1, 0).unwrap();
        let free_before = chunks.free_chunks();

        let mut visited = vec![false; 200];
        chunks.drain(|run| {
            for chunk in run {
                assert!(!visited[chunk], "chunk {chunk} visited twice");
                visited[chunk] = true;
            }
        });

        let expected: Vec<_> =
            (0..200).map(|c| !(a..a + 10).contains(&c) && !(b..b + 70).contains(&c)).collect();
        assert_eq!(visited, expected);
        assert_eq!(chunks.free_chunks(), free_before);
    }

    #[test]
    fn run_starts_agrees_with_a_naive_search() {
        let mut rng = Rng::new(1);
        for _ in 0..iterations(2_000) {
            let free = rng.next_u64() | (rng.next_u64() & rng.next_u64());
            let n = rng.between(1, 64);
            let align = 1 << rng.below(7);
            let naive = (0..=WORD - n)
                .step_by(align)
                .filter(|&s| free & run_mask(n, s) == run_mask(n, s))
                .fold(0u64, |acc, s| acc | 1 << s);
            assert_eq!(run_starts(free, n, align), naive, "free={free:#x} n={n} align={align}");
        }
    }

    #[test]
    fn matches_a_reference_model_under_random_traffic() {
        let mut model = ChunkModel::new(300);
        let mut rng = Rng::new(42);
        for _ in 0..iterations(20_000) {
            if model.live() == 0 || rng.below(100) < 55 {
                let (n, align) = match rng.below(7) {
                    0..=2 => (1, 1),
                    3 => (8, 8),
                    4 => (64, 64),
                    5 => (rng.between(2, 20), 1),
                    _ => (rng.between(65, 140), 1),
                };
                model.alloc(n, align, rng.below(5));
            } else {
                model.free(rng.below(1 << 20));
            }
        }
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use loom::{sync::Arc, thread};

    use crate::testing::Bitmap;

    /// Runs `claim` on two threads against one bitmap and returns what each got.
    fn race<T: Send + 'static>(
        len: usize,
        a: impl FnOnce(&Bitmap) -> T + Send + 'static,
        b: impl FnOnce(&Bitmap) -> T + Send + 'static,
    ) -> (Arc<Bitmap>, T, T) {
        let bitmap = Arc::new(Bitmap::new(len));
        let (x, y) = (bitmap.clone(), bitmap.clone());
        let first = thread::spawn(move || a(&x));
        let second = thread::spawn(move || b(&y));
        (bitmap, first.join().unwrap(), second.join().unwrap())
    }

    #[test]
    fn concurrent_claims_never_hand_out_the_same_chunk() {
        loom::model(|| {
            let claim = |bitmap: &Bitmap| bitmap.chunks().alloc(1, 1, 0).unwrap();
            let (bitmap, a, b) = race(2, claim, claim);
            assert_ne!(a, b);
            assert_eq!(bitmap.chunks().free_chunks(), 0);
        });
    }

    #[test]
    fn exactly_one_thread_wins_the_last_chunk() {
        loom::model(|| {
            let claim = |bitmap: &Bitmap| bitmap.chunks().alloc(1, 1, 0);
            let (bitmap, a, b) = race(1, claim, claim);
            assert!(a.is_some() ^ b.is_some());
            assert_eq!(bitmap.chunks().free_chunks(), 0);
        });
    }

    #[test]
    fn a_free_racing_a_claim_never_loses_the_chunk() {
        loom::model(|| {
            let bitmap = Arc::new(Bitmap::new(1));
            assert_eq!(bitmap.chunks().alloc(1, 1, 0), Some(0));
            let freer = {
                let bitmap = bitmap.clone();
                thread::spawn(move || bitmap.chunks().free(0, 1))
            };
            let claimer = {
                let bitmap = bitmap.clone();
                thread::spawn(move || bitmap.chunks().alloc(1, 1, 0))
            };
            freer.join().unwrap();
            let got = claimer.join().unwrap();
            // Either the claim ran before the free and failed, or after it and won the chunk.
            assert_eq!(bitmap.chunks().free_chunks(), usize::from(got.is_none()));
        });
    }

    #[test]
    fn racing_spans_never_overlap_and_roll_back_cleanly() {
        loom::model(|| {
            let claim = |bitmap: &Bitmap| bitmap.chunks().alloc(65, 1, 0);
            let (bitmap, a, b) = race(192, claim, claim);
            // 65 chunks need two words each and there are three, so at most one fits.
            assert!(a.is_some() ^ b.is_some(), "a={a:?} b={b:?}");
            assert_eq!(bitmap.chunks().free_chunks(), 192 - 65);
        });
    }
}
