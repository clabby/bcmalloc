//! Workloads where blocks live for a while and die out of order, the way they do in most
//! programs.

use super::harness::{Block, Outcome, Rng, Scale, empty_slots, parallel, timed};

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

impl Sizes {
    fn pick(self, rng: &mut Rng) -> usize {
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
    let ops = scale.ops(10_000_000);
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
