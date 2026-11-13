//! Workloads that report how much memory the process holds afterwards and not how fast it got
//! there.

use std::{thread, time::Duration};

use super::{
    churn::{MEDIUM, MIXED, SMALL, Sizes},
    harness::{Block, Outcome, Rng, Scale, empty_slots, resident_mib},
};

/// How long to wait after freeing before looking, so that allocators that return memory on a
/// timer have had their turn.
const SETTLE: Duration = Duration::from_millis(400);

/// Waits, does a little allocation so that allocators which only decide to return memory while
/// they are being used get the chance, and waits again.
fn settle() {
    settle_for(SETTLE);
}

/// [§settle§] with a wait of `delay` on each side.
fn settle_for(delay: Duration) {
    thread::sleep(delay);
    // A running program touches many size classes, which is what makes a lazy allocator look at
    // its free memory.
    for i in 0..10_000 {
        drop(Block::new(16 << (i % 12)));
    }
    thread::sleep(delay);
}

/// Allocates 256 MiB of small blocks, frees every one and reports the memory still held.
pub(crate) fn burst_retention(scale: Scale) -> Outcome {
    let count = scale.ops(262_144);
    let before = resident_mib();
    let blocks: Vec<Block> = (0..count).map(|_| Block::new(1024)).collect();
    drop(blocks);
    settle();
    Outcome::Footprint(resident_mib() - before)
}

/// Allocates 128 MiB of small blocks, frees a random half and reports the memory held. A good
/// allocator can hand back the slabs that emptied and reuse the holes in the others.
pub(crate) fn half_freed(scale: Scale) -> Outcome {
    let count = scale.ops(2_000_000);
    let before = resident_mib();
    let mut rng = Rng::new(50);
    let mut blocks: Vec<Block> = (0..count).map(|_| Block::new(64)).collect();
    rng.shuffle(&mut blocks);
    blocks.truncate(count / 2);
    settle();
    let held = resident_mib() - before;
    drop(blocks);
    Outcome::Footprint(held)
}

/// One phase of [`phase_churn`]: the number of live blocks and the sizes they come in.
struct Phase {
    live: usize,
    sizes: Sizes,
}

const BIG: Sizes = Sizes::Table(&[8 << 10, 16 << 10, 32 << 10, 64 << 10, 128 << 10]);
const HUGE: Sizes = Sizes::Table(&[256 << 10, 512 << 10, 1 << 20]);

/// The phases of [`phase_churn`]. The live set grows, shrinks and changes its sizes, as a program
/// does when its load changes.
const PHASES: [Phase; 6] = [
    Phase { live: 1_000_000, sizes: SMALL },
    Phase { live: 20_000, sizes: MEDIUM },
    Phase { live: 2_000, sizes: BIG },
    Phase { live: 300_000, sizes: MIXED },
    Phase { live: 100, sizes: HUGE },
    Phase { live: 2_000, sizes: SMALL },
];

/// Runs through the phases, which each replace every block of a live set that grows or shrinks to
/// the size of the phase, with the sizes of the phase. After each phase it waits briefly and
/// reads the memory held with the live set still in place. The result is the average of those
/// readings, so an allocator is charged for every phase in which it holds on to memory that an
/// earlier phase left behind and that the live set no longer needs.
pub(crate) fn phase_churn(scale: Scale) -> Outcome {
    let before = resident_mib();
    let mut rng = Rng::new(60);
    let mut live: Vec<Option<Block>> = empty_slots(0);
    let mut total = 0.0;
    for phase in &PHASES {
        let count = scale.ops(phase.live);
        live.truncate(count);
        live.resize_with(count, || None);
        for _ in 0..3 * count {
            let slot = rng.below(count);
            let block = Block::new(phase.sizes.pick(&mut rng));
            block.touch_pages();
            live[slot] = Some(block);
        }
        settle_for(Duration::from_millis(100));
        total += resident_mib() - before;
    }
    drop(live);
    Outcome::Footprint(total / PHASES.len() as f64)
}
