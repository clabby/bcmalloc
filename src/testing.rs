//! Helpers shared by the unit tests, the model-checking tests and the fuzz targets.
//!
//! This module only exists in test builds and behind the `testing` feature, so none of it is part
//! of the allocator.

use std::vec::Vec;

use crate::{chunks::Chunks, sync::AtomicU64};

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
}

impl Bitmap {
    /// Storage that tracks `len` chunks, all free.
    pub(crate) fn new(len: usize) -> Self {
        let words = (0..Chunks::words_for(len)).map(|_| AtomicU64::new(0)).collect();
        Self { words, len }
    }

    /// The allocator over this storage.
    pub(crate) fn chunks(&self) -> Chunks<'_> {
        Chunks::new(&self.words, self.len)
    }
}
