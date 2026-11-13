//! Workloads that report the time of one allocator call and not the rate of many.
//!
//! Each samples the time of a batch of calls and divides by the calls in it, see
//! [`harness::BATCH`], which keeps the clock out of the figure. A run gathers well over a million
//! calls and reports the median, the 99th and the 99.9th percentile of the batches in nanoseconds.
//! Nothing is timed while the live set is built or while blocks are handed to other threads.

use std::sync::{Mutex, mpsc};

use super::{
    churn::MIXED,
    harness::{
        BATCH, Block, Outcome, Rng, Scale, empty_slots, latency, parallel_collect, time_batch,
    },
};

/// Replaces random blocks of a live set of 4096 and times batches of steps. A step is an
/// allocation and the free of the block it replaces, so a batch makes two calls per step.
fn churn_samples(id: usize, steps: usize) -> Vec<u32> {
    let mut rng = Rng::new(0x71 + id as u64);
    let mut live = empty_slots(4096);
    for slot in &mut live {
        *slot = Some(Block::new(MIXED.pick(&mut rng)));
    }
    let batches = steps / BATCH;
    let mut samples = Vec::with_capacity(batches);
    for _ in 0..batches {
        // The choices are made first so that the generator is not in the time.
        let mut picks = [(0, 0); BATCH];
        for pick in &mut picks {
            *pick = (rng.below(live.len()), MIXED.pick(&mut rng));
        }
        samples.push(time_batch(|| {
            for &(slot, size) in &picks {
                live[slot] = Some(Block::new(size));
            }
        }));
    }
    samples
}

/// The latency of allocator calls in the mixed churn of one thread.
pub(crate) fn churn_1_thread(scale: Scale) -> Outcome {
    latency(churn_samples(0, scale.ops(50_000_000)), 2 * BATCH)
}

/// The latency of allocator calls when eight threads each run their own mixed churn. The
/// percentiles cover the batches of all threads.
pub(crate) fn churn_8_threads(scale: Scale) -> Outcome {
    let steps = scale.ops(30_000_000);
    let samples = parallel_collect(8, |id| churn_samples(id, steps)).concat();
    latency(samples, 2 * BATCH)
}

/// Eight producers allocate blocks and send them to eight consumers that free them, so the
/// producers' heaps are always receiving remote frees. Only the allocations of the producers are
/// timed, in batches of 32 that are then sent as one message.
pub(crate) fn remote_frees(scale: Scale) -> Outcome {
    const PAIRS: usize = 8;
    let batches = scale.ops(600_000);
    let (senders, receivers): (Vec<_>, Vec<_>) =
        (0..PAIRS).map(|_| mpsc::sync_channel::<Vec<Block>>(64)).unzip();
    let senders = Mutex::new(senders.into_iter().map(Some).collect::<Vec<_>>());
    let receivers = Mutex::new(receivers.into_iter().map(Some).collect::<Vec<_>>());
    let samples = parallel_collect(PAIRS * 2, |id| {
        let pair = id / 2;
        if id % 2 == 0 {
            let sender = senders.lock().unwrap()[pair].take().unwrap();
            let mut rng = Rng::new(id as u64 + 0x91);
            let mut samples = Vec::with_capacity(batches);
            for _ in 0..batches {
                let mut sizes = [0; BATCH];
                for size in &mut sizes {
                    *size = rng.between(16, 512);
                }
                let mut batch = Vec::with_capacity(BATCH);
                samples.push(time_batch(|| {
                    batch.extend(sizes.iter().map(|&size| Block::new(size)));
                }));
                sender.send(batch).unwrap();
            }
            samples
        } else {
            let receiver = receivers.lock().unwrap()[pair].take().unwrap();
            for batch in receiver {
                drop(batch);
            }
            Vec::new()
        }
    })
    .concat();
    latency(samples, BATCH)
}
