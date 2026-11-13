//! Workloads where several threads share memory, free each other's blocks or live next to each
//! other in the address space.

use std::{
    hint::black_box,
    sync::{
        Mutex,
        atomic::{AtomicPtr, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

use super::harness::{Block, Outcome, Rng, Scale, empty_slots, parallel, raw_alloc, raw_free};

/// The server simulation from the Larson benchmark. Each thread owns a table of blocks,
/// and every generation its table passes to a brand new thread, which frees blocks that another
/// thread allocated. This is the pattern of a thread pool that keeps replacing its workers.
pub(crate) fn larson(scale: Scale) -> Outcome {
    const THREADS: usize = 8;
    let generations = scale.ops(40);
    let ops = 100_000;
    let mut tables: Vec<_> = (0..THREADS).map(|_| empty_slots(1000)).collect();
    let start = Instant::now();
    for generation in 0..generations {
        tables = thread::scope(|scope| {
            let handles: Vec<_> = tables
                .into_iter()
                .enumerate()
                .map(|(id, mut table)| {
                    scope.spawn(move || {
                        let mut rng = Rng::new((generation * THREADS + id) as u64 + 1);
                        for _ in 0..ops {
                            let slot = rng.below(table.len());
                            table[slot] = Some(Block::new(rng.between(16, 1024)));
                        }
                        table
                    })
                })
                .collect();
            handles.into_iter().map(|handle| handle.join().unwrap()).collect()
        });
    }
    let elapsed = start.elapsed();
    drop(tables);
    Outcome::Throughput { ops: (generations * THREADS * ops) as u64, elapsed }
}

/// Threads in a ring hand batches of blocks to the next thread, which frees them. Every free is
/// remote, and the batches are big enough that the hand-off itself is not the bottleneck.
pub(crate) fn ring(threads: usize, scale: Scale) -> Outcome {
    const BATCH: usize = 256;
    const QUEUE: usize = 4;
    let rounds = scale.ops(4000);
    let mailboxes: Vec<Mutex<Vec<Vec<Block>>>> =
        (0..threads).map(|_| Mutex::new(Vec::new())).collect();
    parallel(threads, |id| {
        let mut rng = Rng::new(id as u64 + 100);
        let next = &mailboxes[(id + 1) % threads];
        let (mut sent, mut received) = (0, 0);
        while sent < rounds || received < rounds {
            let batch = mailboxes[id].lock().unwrap().pop();
            let mut idle = batch.is_none();
            if let Some(batch) = batch {
                drop(black_box(batch));
                received += 1;
            }
            if sent < rounds && next.lock().unwrap().len() < QUEUE {
                let batch = (0..BATCH).map(|_| Block::new(rng.between(16, 512))).collect();
                next.lock().unwrap().push(batch);
                sent += 1;
                idle = false;
            }
            if idle {
                thread::yield_now();
            }
        }
        (rounds * BATCH) as u64
    })
}

/// A producer sends blocks over a channel and a consumer frees them, one pair per two threads.
pub(crate) fn channel_pairs(pairs: usize, scale: Scale) -> Outcome {
    let messages = scale.ops(1_000_000);
    let (senders, receivers): (Vec<_>, Vec<_>) =
        (0..pairs).map(|_| mpsc::sync_channel::<Block>(512)).unzip();
    let senders = Mutex::new(senders.into_iter().map(Some).collect::<Vec<_>>());
    let receivers = Mutex::new(receivers.into_iter().map(Some).collect::<Vec<_>>());
    parallel(pairs * 2, |id| {
        let pair = id / 2;
        if id % 2 == 0 {
            let sender = senders.lock().unwrap()[pair].take().unwrap();
            let mut rng = Rng::new(id as u64 + 5);
            for _ in 0..messages {
                sender.send(Block::new(rng.between(32, 512))).unwrap();
            }
            0
        } else {
            let receiver = receivers.lock().unwrap()[pair].take().unwrap();
            for block in receiver {
                black_box(&block);
            }
            messages as u64
        }
    })
}

/// Threads swap blocks through one shared table, so a block is freed by whichever thread draws
/// its slot next. Every thread frees blocks of every other thread at random.
pub(crate) fn shared_table(threads: usize, scale: Scale) -> Outcome {
    let ops = scale.ops(2_000_000);
    let slots: Vec<AtomicPtr<u8>> =
        (0..4096).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect();
    let outcome = parallel(threads, |id| {
        let mut rng = Rng::new(id as u64 + 300);
        for _ in 0..ops {
            let size = rng.between(16, 4096);
            let block = raw_alloc(size);
            // SAFETY: every block holds its own size in its first eight bytes, and the
            // allocation is at least 16 bytes.
            unsafe { block.cast::<usize>().write(size) };
            let old = slots[rng.below(slots.len())].swap(block, Ordering::AcqRel);
            if !old.is_null() {
                // SAFETY: the swap made this thread the only owner of the old block.
                let size = unsafe { old.cast::<usize>().read() };
                raw_free(old, size);
            }
        }
        ops as u64
    });
    for slot in &slots {
        let old = slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: as above, and the threads are done.
            raw_free(old, unsafe { old.cast::<usize>().read() });
        }
    }
    outcome
}

/// Every thread starts, does a little work and exits, so the cost of claiming and releasing a heap
/// shows.
pub(crate) fn thread_lifecycle(scale: Scale) -> Outcome {
    const THREADS: usize = 8;
    let waves = scale.ops(400);
    let start = Instant::now();
    for wave in 0..waves {
        thread::scope(|scope| {
            for id in 0..THREADS {
                scope.spawn(move || {
                    let mut rng = Rng::new((wave * THREADS + id) as u64 + 1);
                    let mut live = empty_slots(64);
                    for _ in 0..2000 {
                        let slot = rng.below(live.len());
                        live[slot] = Some(Block::new(rng.between(16, 1024)));
                    }
                });
            }
        });
    }
    Outcome::Throughput { ops: (waves * THREADS * 2000) as u64, elapsed: start.elapsed() }
}

/// The passive false sharing test from the Hoard paper. The main thread allocates small objects
/// that sit side by side, hands one to each thread, and each thread frees it and then allocates
/// its own and writes to it. An allocator that reuses the freed object for the freeing thread puts
/// every thread's object on one cache line. Throughput is in writes.
pub(crate) fn cache_scratch(threads: usize, scale: Scale) -> Outcome {
    let writes = scale.ops(20_000_000);
    let objects = Mutex::new((0..threads).map(|_| Some(Block::new(8))).collect::<Vec<_>>());
    parallel(threads, |id| {
        let given = objects.lock().unwrap()[id].take();
        drop(given);
        let object = Block::new(8);
        for i in 0..writes {
            object.touch(i as u8);
        }
        writes as u64
    })
}

/// The active false sharing test. Each thread allocates a small object and writes to it. An
/// allocator that hands out neighbouring blocks to different threads makes them fight over a
/// line. Throughput is in writes.
pub(crate) fn cache_thrash(threads: usize, scale: Scale) -> Outcome {
    let writes = scale.ops(20_000_000);
    parallel(threads, |_| {
        let object = Block::new(8);
        for i in 0..writes {
            object.touch(i as u8);
        }
        writes as u64
    })
}
