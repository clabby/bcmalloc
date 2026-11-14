//! A parallel ingest, sort and group-by pipeline with owned, nested records.
//!
//! Workers construct records; sorting redistributes ownership before grouping. The completed
//! groups leave the pool and are dropped on the caller, guaranteeing frees on another thread.

use super::{
    Pass, Scale,
    text::{Rng, WORDS},
    twice,
};
use rayon::prelude::*;
use std::{collections::HashMap, hint::black_box, time::Instant};

struct Record {
    key: String,
    tags: HashMap<String, String>,
    values: Vec<u64>,
}

pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(1, usize::from).min(32))
        .build()
        .expect("a thread pool");
    let count = scale.count(1_200_000);
    twice(|| {
        let start = Instant::now();
        let groups = pool.install(|| {
            let mut records: Vec<Record> = (0..count)
                .into_par_iter()
                .map(|id| {
                    let mut rng = Rng::new(id as u64 + 0x5241);
                    Record {
                        key: format!("{}-{:08x}", rng.pick(WORDS), rng.next() as u32),
                        tags: (0..4)
                            .map(|_| (rng.pick(WORDS).to_string(), format!("value-{}", rng.next())))
                            .collect(),
                        values: (0..rng.between(4, 24)).map(|_| rng.next()).collect(),
                    }
                })
                .collect();
            records.par_sort_unstable_by(|a, b| a.key.cmp(&b.key));
            records
                .into_par_iter()
                .fold(HashMap::<String, Vec<Record>>::new, |mut groups, record| {
                    let group = record.key.split('-').next().expect("a prefix").to_string();
                    groups.entry(group).or_default().push(record);
                    groups
                })
                .reduce(HashMap::new, |mut left, right| {
                    for (key, mut records) in right {
                        left.entry(key).or_default().append(&mut records);
                    }
                    left
                })
        });
        assert_eq!(groups.values().map(Vec::len).sum::<usize>(), count);
        let checksum: usize =
            groups.values().flatten().map(|r| r.tags.len() + r.values.len()).sum();
        black_box(checksum);
        drop(groups);
        Pass::since(start, count as f64)
    })
}
