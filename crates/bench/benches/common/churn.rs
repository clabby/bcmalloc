//! Workloads where blocks live for a while and die out of order, the way they do in most
//! programs.

use super::harness::{Block, Outcome, Rng, Scale, empty_slots, parallel, timed};
use std::{hint::black_box, thread};

/// How a churn workload picks request sizes.
#[derive(Clone, Copy)]
pub(crate) enum Sizes {
    /// One of these, uniformly.
    Table(&'static [usize]),
    /// Sizes from 16 bytes to about 32 KiB where each doubling is half as likely as the last.
    Skewed,
}

pub(crate) const SMALL: Sizes = Sizes::Table(&[16, 24, 32, 48, 64, 80, 96, 128]);
pub(crate) const MEDIUM: Sizes = Sizes::Table(&[128, 192, 256, 384, 512, 768, 1024, 2048]);
pub(crate) const MIXED: Sizes =
    Sizes::Table(&[16, 24, 32, 48, 64, 64, 96, 128, 256, 512, 1024, 4096]);
/// Sizes from 16 bytes to 16 KiB, which average a little over 2 KiB, so that a big live set has
/// few enough blocks to be built in a moment.
pub(crate) const WIDE: Sizes =
    Sizes::Table(&[16, 32, 48, 64, 96, 128, 192, 256, 512, 1024, 2048, 4096, 8192, 16384]);

impl Sizes {
    pub(crate) fn pick(self, rng: &mut Rng) -> usize {
        match self {
            Self::Table(table) => table[rng.below(table.len())],
            Self::Skewed => {
                let doublings = (rng.next().trailing_zeros() as usize).min(11);
                let base = 16 << doublings;
                base + rng.below(base)
            }
        }
    }
}

/// Each thread keeps 4096 live blocks and keeps replacing random ones.
pub(crate) fn churn(threads: usize, sizes: Sizes, scale: Scale) -> Outcome {
    churn_ops(threads, sizes, scale.ops(10_000_000))
}

/// The churn of [`churn`] with `ops` replacements on every thread.
fn churn_ops(threads: usize, sizes: Sizes, ops: usize) -> Outcome {
    let body = |id: usize| {
        let mut rng = Rng::new(0x51 + id as u64);
        let mut live = empty_slots(4096);
        for _ in 0..ops {
            let slot = rng.below(live.len());
            live[slot] = Some(Block::new(sizes.pick(&mut rng)));
        }
        ops as u64
    };
    if threads == 1 { timed(|| body(0)) } else { parallel(threads, body) }
}

/// Churn with four times as many threads as the machine has cores, which is a server that runs
/// more workers than cores. The scheduler preempts threads in the middle of allocator calls, and
/// every thread still wants its own heap. The total work is fixed, so the figure does not depend
/// on the thread count.
pub(crate) fn churn_oversubscribed(scale: Scale) -> Outcome {
    let threads = 4 * thread::available_parallelism().map_or(1, usize::from);
    churn_ops(threads, MIXED, scale.ops(1_000_000_000) / threads)
}

/// Fills a live set of about 2 GiB with blocks of 16 bytes to 16 KiB, writing every page, and then
/// replaces random blocks until it has replaced twelve times as many as it holds. The live set is
/// far bigger than any cache, so every call touches cold memory, and the allocator has to find a
/// free block among a million. One operation is a block allocated, and the fill counts.
pub(crate) fn churn_large_set(scale: Scale) -> Outcome {
    let target = scale.ops(2 << 30);
    timed(|| {
        let mut rng = Rng::new(0x1a);
        let mut live: Vec<Option<Block>> = Vec::with_capacity(target / 1024);
        let mut bytes = 0;
        while bytes < target {
            let size = WIDE.pick(&mut rng);
            let block = Block::new(size);
            block.touch_pages();
            live.push(Some(block));
            bytes += size;
        }
        let replacements = 12 * live.len();
        for _ in 0..replacements {
            let slot = rng.below(live.len());
            let block = Block::new(WIDE.pick(&mut rng));
            block.touch_pages();
            live[slot] = Some(block);
        }
        (live.len() + replacements) as u64
    })
}

/// How many rounds of the busy loop one operation of [`churn_with_work`] spends.
const WORK_ROUNDS: u32 = 60;

/// A chain of dependent steps, which stands for the computation a program does with a block. The
/// result depends on every step, so the compiler cannot drop the loop.
fn busy_work(mut state: u64) -> u64 {
    for _ in 0..black_box(WORK_ROUNDS) {
        state = (state ^ (state >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }
    state
}

/// The mixed churn of one thread with work between the allocator calls. Each new block is filled
/// and hashed, and then a few hundred nanoseconds of computation follow. Real programs spend most
/// of their time outside the allocator, and this shows how much of an allocator's lead on the
/// pure churn survives that.
pub(crate) fn churn_with_work(scale: Scale) -> Outcome {
    let ops = scale.ops(2_000_000);
    timed(|| {
        let mut rng = Rng::new(0x77);
        let mut live = empty_slots(4096);
        let mut state = 1;
        for i in 0..ops {
            let slot = rng.below(live.len());
            let mut block = Block::new(MIXED.pick(&mut rng));
            state ^= block.fill_and_hash(i as u64);
            state = busy_work(state);
            live[slot] = Some(block);
        }
        black_box(state);
        ops as u64
    })
}

/// Blocks of 64 KiB to 1 MiB with only a few live at a time, so the allocator has to decide
/// whether to keep the memory or give it back.
pub(crate) fn medium_blocks(scale: Scale) -> Outcome {
    let ops = scale.ops(200_000);
    timed(|| {
        let mut rng = Rng::new(3);
        let mut live = empty_slots(64);
        for _ in 0..ops {
            let slot = rng.below(live.len());
            let block = Block::new(rng.between(64 << 10, 1 << 20));
            block.touch_pages();
            live[slot] = Some(block);
        }
        ops as u64
    })
}

/// Blocks from a quarter of a megabyte to sixteen, touched on every page and freed at once.
pub(crate) fn large_blocks(scale: Scale) -> Outcome {
    const SIZES: [usize; 4] = [256 << 10, 1 << 20, 4 << 20, 16 << 20];
    let reps = scale.ops(800);
    timed(|| {
        for i in 0..reps {
            let block = Block::new(SIZES[i % SIZES.len()]);
            block.touch_pages();
        }
        reps as u64
    })
}
