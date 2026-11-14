//! Single-thread microbenchmarks that isolate the cost of one allocation path.

use super::harness::{Block, Outcome, Rng, Scale, layout, raw_alloc, raw_free, timed};
use std::hint::black_box;

/// Allocates and frees one block of `SIZE` bytes over and over. This is the hot path in its purest
/// form, with the block coming straight back from the free list it was just pushed on.
pub(crate) fn pair<const SIZE: usize>(scale: Scale) -> Outcome {
    let base = match SIZE {
        0..=4096 => 20_000_000,
        4097..=65536 => 5_000_000,
        65537..=262_144 => 1_000_000,
        _ => 100_000,
    };
    let pairs = scale.ops(base);
    timed(|| {
        for _ in 0..pairs {
            let ptr = raw_alloc(black_box(SIZE));
            // SAFETY: the allocation has at least one byte.
            unsafe { ptr.write_volatile(1) };
            raw_free(black_box(ptr), SIZE);
        }
        pairs as u64
    })
}

/// Allocates a large batch and then frees all of it. `SHUFFLE` frees in a random order, which
/// scatters the frees over every slab the batch touched.
pub(crate) fn bulk<const SIZE: usize, const SHUFFLE: bool>(scale: Scale) -> Outcome {
    let count = scale.ops((256 << 20) / SIZE).min(scale.ops(1_000_000));
    let rounds = 3;
    let mut rng = Rng::new(7);
    let mut elapsed = std::time::Duration::ZERO;
    for _ in 0..rounds {
        let mut blocks = Vec::with_capacity(count);
        let start = std::time::Instant::now();
        blocks.extend((0..count).map(|_| Block::new(SIZE)));
        elapsed += start.elapsed();
        if SHUFFLE {
            rng.shuffle(&mut blocks);
        }
        let start = std::time::Instant::now();
        drop(blocks);
        elapsed += start.elapsed();
    }
    Outcome::Throughput { ops: (count * rounds) as u64, elapsed }
}

/// Resizes blocks that are all live at once to random sizes, which exercises in-place growth and
/// the copy that happens when it is not possible.
pub(crate) fn realloc_random(scale: Scale) -> Outcome {
    let steps = scale.ops(10_000_000);
    timed(|| {
        let mut rng = Rng::new(11);
        let mut live: Vec<(*mut u8, usize)> = (0..1024).map(|_| (raw_alloc(64), 64)).collect();
        for _ in 0..steps {
            let slot = rng.below(live.len());
            let (ptr, size) = live[slot];
            let new_size = rng.between(16, 16 << 10);
            // SAFETY: the pointer is live with this size, and the new size is not zero.
            let moved = unsafe { std::alloc::realloc(ptr, layout(size), new_size) };
            assert!(!moved.is_null(), "out of memory");
            // SAFETY: the block has at least one byte.
            unsafe { moved.write_volatile(1) };
            live[slot] = (moved, new_size);
        }
        for (ptr, size) in live {
            raw_free(ptr, size);
        }
        steps as u64
    })
}
