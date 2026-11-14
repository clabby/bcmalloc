//! A shared embedded LSM database with a cache-sized live set.
//!
//! Population is outside the clock. Timed operations mix writes, reads, short scans and deletes;
//! journal writes are buffered, so this measures a service's cache path rather than fsync latency.

use super::{Pass, Scale, text::Rng, twice};
use fjall::{Database, KeyspaceCreateOptions};
use std::{hint::black_box, thread, time::Instant};

const KEYS: usize = 65_536;
const VALUE_BYTES: usize = 4096;

fn pass(operations: usize) -> Pass {
    let directory = tempfile::tempdir().expect("a database directory");
    let database = Database::builder(directory.path())
        .cache_size(256 * 1024 * 1024)
        .open()
        .expect("open database");
    let items = database
        .keyspace("records", || {
            KeyspaceCreateOptions::default().max_memtable_size(512 * 1024 * 1024)
        })
        .expect("keyspace");
    let mut rng = Rng::new(0x4b56);
    let values: Vec<Vec<u8>> =
        (0..256).map(|_| (0..VALUE_BYTES).map(|_| rng.next() as u8).collect()).collect();
    for key in 0..KEYS {
        items.insert((key as u64).to_be_bytes(), &values[key % values.len()]).expect("populate");
    }
    let workers = thread::available_parallelism().map_or(1, usize::from).min(16);
    let start = Instant::now();
    thread::scope(|scope| {
        for worker in 0..workers {
            let items = &items;
            let values = &values;
            scope.spawn(move || {
                let mut rng = Rng::new(0x5354 + worker as u64);
                for _ in (worker..operations).step_by(workers) {
                    let key = (rng.below(KEYS) as u64).to_be_bytes();
                    match rng.below(10) {
                        0..=3 => {
                            items.insert(key, &values[rng.below(values.len())]).expect("insert")
                        }
                        4..=7 => {
                            black_box(items.get(key).expect("read"));
                        }
                        8 => {
                            for entry in items.range(key..).take(16) {
                                black_box(entry.into_inner().expect("scan"));
                            }
                        }
                        _ => items.remove(key).expect("delete"),
                    }
                }
            });
        }
    });
    let pass = Pass::since(start, operations as f64);
    // Close the storage engine before removing the directory, including its background workers.
    drop(items);
    drop(database);
    directory.close().expect("remove database");
    pass
}

pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    twice(|| pass(scale.count(160_000)))
}
