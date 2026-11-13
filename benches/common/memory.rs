//! Workloads that report how much memory the process holds afterwards and not how fast it got
//! there.

use std::{thread, time::Duration};

use super::harness::{Block, Outcome, Rng, Scale, resident_mib};

/// How long to wait after freeing before looking, so that allocators that return memory on a
/// timer have had their turn.
const SETTLE: Duration = Duration::from_millis(400);

/// Waits, does a little allocation so that allocators which only decide to return memory while
/// they are being used get the chance, and waits again.
fn settle() {
    thread::sleep(SETTLE);
    for _ in 0..10_000 {
        drop(Block::new(64));
    }
    thread::sleep(SETTLE);
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
