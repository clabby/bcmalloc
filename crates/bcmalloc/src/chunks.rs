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
//! # Giving memory back
//!
//! Next to the bitmap of used chunks are a bitmap of chunks that were freed since their pages were
//! last given back, a bitmap of chunks whose pages are gone, and a timestamp for every word. A
//! purge pass claims the dirty, free chunks of every word that has been quiet for the delay,
//! through the same compare-and-swap that allocation uses, so it can never take a chunk in use. The
//! caller returns the pages to the OS, and the next claim of a purged chunk learns that it must
//! prepare them for use again.
//!
//! Pages are given back in granules of a fixed number of chunks. With transparent huge pages a
//! granule is one 2 MiB page, 32 chunks, and a pass only takes a granule whose every chunk is free.
//! Returning part of a huge page would split it into small pages, which costs the TLB reach that
//! huge pages exist for. This is the rule Temeraire follows in TCMalloc.
//!
//! # Placement within a word
//!
//! A word of 64 chunks covers two huge pages. When a run fits in either half, it goes to the half
//! that is already fuller, so that allocations pack into partly used huge pages and leave empty
//! ones empty, where a purge can take them whole.
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

/// The bitmaps of chunks. See the [module documentation](self).
#[derive(Debug)]
pub(crate) struct Chunks<'a> {
    /// One bit per chunk, set while the chunk is allocated.
    used: &'a [AtomicU64],
    /// Set for chunks that have been freed since they were last purged, so their pages may still
    /// be resident. Chunks that were never used are not set, because they hold no pages.
    dirty: &'a [AtomicU64],
    /// Set for chunks whose pages have been given back to the operating system.
    purged: &'a [AtomicU64],
    /// For each word, the latest time that one of its chunks was freed, in milliseconds.
    stamps: &'a [AtomicU64],
    len: usize,
    /// How many chunks are purged together, a power of two of at most 64.
    granule: usize,
}

impl<'a> Chunks<'a> {
    /// The number of bitmap words needed to track `chunks` chunks.
    pub(crate) const fn words_for(chunks: usize) -> usize {
        chunks.div_ceil(WORD)
    }

    /// The number of words of storage needed to track `chunks` chunks, for all four arrays.
    pub(crate) const fn storage_words(chunks: usize) -> usize {
        4 * Self::words_for(chunks)
    }

    /// Wraps zeroed bitmap storage that tracks `len` chunks, purging `granule` aligned chunks at
    /// a time. `granule` must be a power of two of at most 64.
    ///
    /// # Panics
    ///
    /// Panics if `words` is shorter than [`Chunks::storage_words`] for `len`, or `granule` is not
    /// a power of two of at most 64.
    pub(crate) fn with_granule(words: &'a [AtomicU64], len: usize, granule: usize) -> Self {
        assert!(granule.is_power_of_two() && granule <= WORD, "bad purge granule {granule}");
        let each = Self::words_for(len);
        assert!(words.len() >= 4 * each, "bitmap too short for {len} chunks");
        let (used, rest) = words.split_at(each);
        let (dirty, rest) = rest.split_at(each);
        let (purged, rest) = rest.split_at(each);
        Self { used, dirty, purged, stamps: &rest[..each], len, granule }
    }

    /// The number of chunks tracked.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// The number of chunks currently free.
    pub(crate) fn free_chunks(&self) -> usize {
        let allocated: usize = self
            .used
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
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn alloc(&self, n: usize, align: usize, hint: usize) -> Option<usize> {
        self.alloc_near(n, align, hint, 0..Self::words_for(self.len))
    }

    /// As [`alloc`](Self::alloc), but looks in the bitmap words of `home` first and only then in
    /// the whole bitmap. This is how a heap keeps to the memory of its own NUMA node.
    pub(crate) fn alloc_near(
        &self,
        n: usize,
        align: usize,
        hint: usize,
        home: Range<usize>,
    ) -> Option<usize> {
        debug_assert!(n > 0 && align.is_power_of_two() && align <= WORD);
        if n > self.len {
            return None;
        }
        let words = Self::words_for(self.len);
        let home = home.start.min(words)..home.end.min(words);
        if !home.is_empty() &&
            home.len() < words &&
            let Some(first) = self.alloc_in(n, align, hint, home)
        {
            return Some(first);
        }
        self.alloc_in(n, align, hint, 0..words)
    }

    /// Claims a run whose words all lie in `words`, as [`alloc`](Self::alloc) places them.
    fn alloc_in(&self, n: usize, align: usize, hint: usize, words: Range<usize>) -> Option<usize> {
        let count = words.len();
        match n {
            1 => {
                let start = hint.wrapping_sub(words.start) % count;
                let start = if words.contains(&hint) || words.start == 0 { start } else { 0 };
                (0..count)
                    .map(|i| words.start + (start + i) % count)
                    .find_map(|w| self.claim(w, 1, align, false))
            }
            2..=WORD => words.rev().find_map(|w| self.claim(w, n, align, true)),
            _ => self.claim_span(n, words),
        }
    }

    /// Claims exactly the `n` chunks from `first` if every one of them is free, and returns whether
    /// it did. A run that spans words is claimed one word at a time and given back if a later word
    /// is taken, so this is obstruction-free like the claim of a long run.
    pub(crate) fn claim_at(&self, first: usize, n: usize) -> bool {
        debug_assert!(n > 0 && first + n <= self.len);
        let end = first + n;
        let mut chunk = first;
        while chunk < end {
            let (word, offset) = (chunk / WORD, chunk % WORD);
            let take = (WORD - offset).min(end - chunk);
            let mask = run_mask(take, offset);
            let used = &self.used[word];
            let mut seen = used.load(Ordering::Relaxed);
            loop {
                if seen & mask != 0 {
                    // Give back what this call already claimed.
                    if chunk > first {
                        self.release(first, chunk - first);
                    }
                    return false;
                }
                match used.compare_exchange_weak(
                    seen,
                    seen | mask,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => seen = actual,
                }
            }
            chunk += take;
        }
        true
    }

    /// Clears the bits of chunks that a failed claim took, without marking them dirty or touching
    /// their clock, since nothing was ever stored in them.
    fn release(&self, first: usize, n: usize) {
        let end = first + n;
        let mut chunk = first;
        while chunk < end {
            let (word, offset) = (chunk / WORD, chunk % WORD);
            let take = (WORD - offset).min(end - chunk);
            self.used[word].fetch_and(!run_mask(take, offset), Ordering::Release);
            chunk += take;
        }
    }

    /// Returns `n` chunks starting at `first`, which were freed at time `now` in milliseconds.
    ///
    /// The run must be exactly one previously returned by [`alloc`](Self::alloc), or a part of
    /// one. The time starts the clock that decides when the chunks are purged.
    pub(crate) fn free_at(&self, first: usize, n: usize, now: u64) {
        debug_assert!(n > 0 && first + n <= self.len);
        let mut chunk = first;
        let end = first + n;
        while chunk < end {
            let (word, offset) = (chunk / WORD, chunk % WORD);
            let take = (WORD - offset).min(end - chunk);
            let mask = run_mask(take, offset);
            // These change before the chunks become free, so that a pass that sees the chunks free
            // also sees them dirty. A pass that read the time before it moved may still take the
            // chunks at once, which costs them a page fault and nothing else.
            self.dirty[word].fetch_or(mask, Ordering::Relaxed);
            self.stamps[word].fetch_max(now, Ordering::Relaxed);
            let before = self.used[word].fetch_and(!mask, Ordering::Release);
            debug_assert_eq!(before & mask, mask, "freeing chunks that are not allocated");
            chunk += take;
        }
    }

    /// Returns `n` chunks starting at `first` as [`Chunks::free_at`] does, with the clock at zero.
    #[cfg(test)]
    pub(crate) fn free(&self, first: usize, n: usize) {
        self.free_at(first, n, 0);
    }

    /// Claims every free chunk one word at a time, hands each maximal free run to `visit`, records
    /// that the runs were purged, and gives the chunks back.
    ///
    /// The caller returns the runs' pages to the OS in `visit`, which is why they count as purged.
    /// While `visit` runs the run belongs to the caller, so nobody can allocate and use it at the
    /// same time. The cost is that other threads see up to 64 chunks of free memory as missing
    /// while a word is being drained.
    pub(crate) fn drain(&self, mut visit: impl FnMut(Range<usize>)) {
        for (w, word) in self.used.iter().take(Self::words_for(self.len)).enumerate() {
            let valid = self.valid_mask(w);
            let claimed = !word.fetch_or(valid, Ordering::AcqRel) & valid;
            let mut rest = claimed;
            while rest != 0 {
                let start = rest.trailing_zeros() as usize;
                let run = (rest >> start).trailing_ones() as usize;
                visit(w * WORD + start..w * WORD + start + run);
                rest &= !run_mask(run, start);
            }
            if claimed != 0 {
                self.purged[w].fetch_or(claimed, Ordering::Relaxed);
                self.dirty[w].fetch_and(!claimed, Ordering::Relaxed);
                word.fetch_and(!claimed, Ordering::Release);
            }
        }
    }

    /// One pass of the purge clock at time `now` in milliseconds. Free granules with a dirty chunk,
    /// in a word whose latest free is at least `delay` old, are claimed and handed to `visit` in
    /// maximal runs. Returns whether such granules remain because their word was freed into too
    /// recently. Dirty chunks in a granule that is partly in use wait until it is wholly free.
    ///
    /// Only chunks that this pass has claimed are visited, so `visit` may return their pages to the
    /// OS without risk to a live allocation. The claims are release operations, so a thread that
    /// sees a chunk missing because of this pass also sees anything the caller stored before the
    /// pass began.
    pub(crate) fn purge_pass(
        &self,
        now: u64,
        delay: u64,
        mut visit: impl FnMut(Range<usize>),
    ) -> bool {
        let mut pending = false;
        for w in 0..Self::words_for(self.len) {
            let valid = self.valid_mask(w);
            if self.dirty[w].load(Ordering::Relaxed) & valid == 0 {
                continue;
            }
            if now.saturating_sub(self.stamps[w].load(Ordering::Relaxed)) < delay {
                let free = !self.used[w].load(Ordering::Relaxed) & valid;
                pending |= self.purgeable(free, self.dirty[w].load(Ordering::Relaxed), valid) != 0;
                continue;
            }
            // Claim the free granules that hold a dirty chunk. A dirty chunk in use was reclaimed
            // after it was freed and becomes dirty again when it is freed.
            let mut seen = self.used[w].load(Ordering::Relaxed);
            let claimed = loop {
                let want =
                    self.purgeable(!seen & valid, self.dirty[w].load(Ordering::Relaxed), valid);
                if want == 0 {
                    break 0;
                }
                match self.used[w].compare_exchange_weak(
                    seen,
                    seen | want,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break want,
                    Err(actual) => seen = actual,
                }
            };
            let mut rest = claimed;
            while rest != 0 {
                let start = rest.trailing_zeros() as usize;
                let run = (rest >> start).trailing_ones() as usize;
                visit(w * WORD + start..w * WORD + start + run);
                rest &= !run_mask(run, start);
            }
            if claimed != 0 {
                self.purged[w].fetch_or(claimed, Ordering::Relaxed);
                self.dirty[w].fetch_and(!claimed, Ordering::Relaxed);
                self.used[w].fetch_and(!claimed, Ordering::Release);
            }
        }
        pending
    }

    /// The chunks of a word that a purge would take: every granule that is entirely `free` and has
    /// a `dirty` chunk. A granule that runs past the end of the region counts its valid part.
    const fn purgeable(&self, free: u64, dirty: u64, valid: u64) -> u64 {
        if self.granule == 1 {
            return dirty & free & valid;
        }
        let mut want = 0;
        let mut offset = 0;
        while offset < WORD {
            let mask = run_mask(self.granule, offset) & valid;
            if mask != 0 && free & mask == mask && dirty & mask != 0 {
                want |= mask;
            }
            offset += self.granule;
        }
        want
    }

    /// Hands the purged runs among the `n` chunks from `first`, which the caller has just claimed,
    /// to `visit`, and records that they are in use again. The caller makes the pages ready for
    /// reuse where the system needs that.
    pub(crate) fn take_purged(&self, first: usize, n: usize, mut visit: impl FnMut(Range<usize>)) {
        let end = first + n;
        let mut chunk = first;
        while chunk < end {
            let (word, offset) = (chunk / WORD, chunk % WORD);
            let take = (WORD - offset).min(end - chunk);
            let mask = run_mask(take, offset);
            if self.purged[word].load(Ordering::Relaxed) & mask != 0 {
                let mut rest = self.purged[word].fetch_and(!mask, Ordering::Relaxed) & mask;
                while rest != 0 {
                    let start = rest.trailing_zeros() as usize;
                    let run = (rest >> start).trailing_ones() as usize;
                    visit(word * WORD + start..word * WORD + start + run);
                    rest &= !run_mask(run, start);
                }
            }
            chunk += take;
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
        let word = &self.used[w];
        let unusable = !self.valid_mask(w);
        let mut seen = word.load(Ordering::Relaxed);
        loop {
            let starts = run_starts(!(seen | unusable), n, align);
            if starts == 0 {
                return None;
            }
            let offset = pick_start(starts, seen, highest);
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
    fn claim_span(&self, n: usize, words: Range<usize>) -> Option<usize> {
        let (full, rest) = (n / WORD, n % WORD);
        let end = (words.end * WORD).min(self.len);
        if end < words.start * WORD + n {
            return None;
        }
        (words.start..=(end - n) / WORD)
            .rev()
            .find(|&first| {
                self.looks_free(first, full, rest) && self.try_claim_span(first, full, rest)
            })
            .map(|first| first * WORD)
    }

    /// A cheap, racy pre-check that avoids claiming words that are obviously taken.
    fn looks_free(&self, first: usize, full: usize, rest: usize) -> bool {
        self.used[first..first + full].iter().all(|w| w.load(Ordering::Relaxed) == 0) &&
            (rest == 0 ||
                self.used[first + full].load(Ordering::Relaxed) & run_mask(rest, 0) == 0)
    }

    /// Claims `full` whole words and the low `rest` bits of the next, undoing everything if any
    /// of them is taken.
    fn try_claim_span(&self, first: usize, full: usize, rest: usize) -> bool {
        for (claimed, word) in self.used[first..first + full].iter().enumerate() {
            if word.compare_exchange(0, u64::MAX, Ordering::Acquire, Ordering::Relaxed).is_err() {
                self.undo(first, claimed);
                return false;
            }
        }
        if rest > 0 {
            let (word, mask) = (&self.used[first + full], run_mask(rest, 0));
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
        for word in &self.used[first..first + claimed] {
            word.store(0, Ordering::Release);
        }
    }
}

/// A mask of `n` one bits starting at bit `offset`.
const fn run_mask(n: usize, offset: usize) -> u64 {
    if n == WORD { u64::MAX } else { ((1 << n) - 1) << offset }
}

/// Chooses where a run starts among `starts`, given the word's `used` bits. A start in the fuller
/// half of the word wins, and within a half, or when the halves are equally full, the highest start
/// if `highest` and the lowest otherwise.
const fn pick_start(starts: u64, used: u64, highest: bool) -> usize {
    const LOW: u64 = u32::MAX as u64;
    let (low, high) = (starts & LOW, starts & !LOW);
    let starts = if low != 0 && high != 0 {
        let (low_used, high_used) = ((used & LOW).count_ones(), (used & !LOW).count_ones());
        if low_used > high_used {
            low
        } else if high_used > low_used {
            high
        } else {
            starts
        }
    } else {
        starts
    };
    (if highest { 63 - starts.leading_zeros() } else { starts.trailing_zeros() }) as usize
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
    fn chunks_are_purged_once_their_word_has_been_quiet_for_the_delay() {
        let bitmap = Bitmap::new(8);
        let chunks = bitmap.chunks();
        let first = chunks.alloc(4, 1, 0).unwrap();
        chunks.free_at(first, 4, 100);
        assert!(chunks.purge_pass(105, 10, |_| panic!("too early")), "the chunks are waiting");
        let mut purged = Vec::new();
        assert!(!chunks.purge_pass(110, 10, |run| purged.push(run)));
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0], first..first + 4);
        assert_eq!(chunks.free_chunks(), 8, "purging gives the chunks back");
        chunks.purge_pass(1000, 10, |_| panic!("purged chunks are not purged again"));
    }

    #[test]
    fn a_later_free_into_the_word_delays_the_purge_of_all_of_it() {
        let bitmap = Bitmap::new(8);
        let chunks = bitmap.chunks();
        let a = chunks.alloc(1, 1, 0).unwrap();
        let b = chunks.alloc(1, 1, 0).unwrap();
        chunks.free_at(a, 1, 100);
        chunks.free_at(b, 1, 108);
        chunks.purge_pass(112, 10, |_| panic!("the word was freed into at 108"));
        let mut purged = Vec::new();
        chunks.purge_pass(118, 10, |run| purged.push(run));
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0], a..b + 1);
    }

    #[test]
    fn a_chunk_that_is_in_use_is_never_purged() {
        let bitmap = Bitmap::new(8);
        let chunks = bitmap.chunks();
        let first = chunks.alloc(1, 1, 0).unwrap();
        chunks.free_at(first, 1, 100);
        assert_eq!(chunks.alloc(1, 1, 0), Some(first));
        chunks.purge_pass(1000, 10, |_| panic!("the chunk is in use"));
        chunks.free_at(first, 1, 1000);
        let mut purged = Vec::new();
        chunks.purge_pass(1010, 10, |run| purged.push(run));
        assert_eq!(purged.len(), 1);
    }

    #[test]
    fn claiming_purged_chunks_reports_each_run_once() {
        let bitmap = Bitmap::new(8);
        let chunks = bitmap.chunks();
        let first = chunks.alloc(8, 1, 0).unwrap();
        chunks.free_at(first, 8, 0);
        chunks.purge_pass(10, 10, |_| ());
        assert_eq!(chunks.alloc(3, 1, 0), Some(5), "runs of several chunks come from the top");
        let mut reported = Vec::new();
        chunks.take_purged(5, 3, |run| reported.push(run));
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0], 5..8);
        chunks.take_purged(5, 3, |_| panic!("already reported"));
    }

    #[test]
    fn a_claim_at_a_place_takes_exactly_free_runs_and_rolls_back() {
        let bitmap = Bitmap::new(200);
        let chunks = bitmap.chunks();
        assert!(chunks.claim_at(60, 70), "a free run across three words");
        assert_eq!(chunks.free_chunks(), 130);
        assert!(!chunks.claim_at(10, 51), "chunk 60 is taken");
        assert!(!chunks.claim_at(129, 40), "chunk 129 is taken");
        assert_eq!(chunks.free_chunks(), 130, "a failed claim leaves nothing behind");
        assert!(chunks.claim_at(130, 70));
        assert_eq!(chunks.free_chunks(), 60);
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
        for granule in [1, 32] {
            let mut model = ChunkModel::with_granule(300, granule);
            let mut rng = Rng::new(42);
            for _ in 0..iterations(20_000) {
                random_step(&mut model, &mut rng);
            }
        }
    }

    fn random_step(model: &mut ChunkModel, rng: &mut Rng) {
        {
            if rng.below(100) < 4 {
                model.purge(rng.below(12) as u64);
            } else if model.live() == 0 || rng.below(100) < 55 {
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

    #[test]
    fn a_huge_page_is_purged_only_when_all_of_it_is_free() {
        let bitmap = Bitmap::with_granule(64, 32);
        let chunks = bitmap.chunks();
        let singles: Vec<_> = (0..33).map(|_| chunks.alloc(1, 1, 0).unwrap()).collect();
        assert_eq!(singles, (0..33).collect::<Vec<_>>());
        // Free all but one chunk of the first huge page, and the one chunk of the second.
        for &chunk in &singles[1..] {
            chunks.free_at(chunk, 1, 0);
        }
        let mut purged = Vec::new();
        assert!(!chunks.purge_pass(100, 10, |run| purged.push(run)));
        assert_eq!(purged.len(), 1, "only the wholly free huge page goes back");
        assert_eq!(purged[0], 32..64);
        chunks.free_at(0, 1, 100);
        purged.clear();
        chunks.purge_pass(200, 10, |run| purged.push(run));
        assert_eq!(purged.first(), Some(&(0..32)));
    }

    #[test]
    fn single_chunks_fill_the_fuller_huge_page_of_a_word_first() {
        let bitmap = Bitmap::new(64);
        let chunks = bitmap.chunks();
        assert_eq!(chunks.alloc(8, 8, 0), Some(56), "the run takes the top of the word");
        // The high half holds the run, so singles fill it before they start on the empty low half.
        for chunk in 32..56 {
            assert_eq!(chunks.alloc(1, 1, 0), Some(chunk));
        }
        assert_eq!(chunks.alloc(1, 1, 0), Some(0), "the high half is full");
        chunks.free_at(40, 1, 0);
        assert_eq!(chunks.alloc(1, 1, 0), Some(40), "the high half is fuller than the low one");
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
    fn a_purge_pass_never_takes_a_chunk_that_a_claim_won() {
        use loom::sync::atomic::{AtomicBool, Ordering};

        loom::model(|| {
            let bitmap = Arc::new(Bitmap::new(2));
            let held = Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
            // Both chunks have been free for longer than the delay when the race starts.
            assert_eq!(bitmap.chunks().alloc(2, 1, 0), Some(0));
            bitmap.chunks().free_at(0, 2, 0);

            let purger = {
                let (bitmap, held) = (bitmap.clone(), held.clone());
                thread::spawn(move || {
                    bitmap.chunks().purge_pass(100, 10, |run| {
                        for chunk in run {
                            assert!(!held[chunk].load(Ordering::SeqCst), "purged a chunk in use");
                        }
                    });
                })
            };
            let claimer = {
                let (bitmap, held) = (bitmap.clone(), held);
                thread::spawn(move || {
                    // The pass may hold both chunks at this moment, so the claim can fail.
                    let chunk = bitmap.chunks().alloc(1, 1, 0);
                    if let Some(chunk) = chunk {
                        held[chunk].store(true, Ordering::SeqCst);
                    }
                    chunk
                })
            };
            purger.join().unwrap();
            let claimed = claimer.join().unwrap();
            assert_eq!(bitmap.chunks().free_chunks(), 2 - usize::from(claimed.is_some()));
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
