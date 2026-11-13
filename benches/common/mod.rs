//! The workloads shared by every allocator benchmark.
//!
//! A program has exactly one global allocator, so each allocator gets its own benchmark binary
//! that installs it and calls [`run`]. A binary runs one workload per invocation, which keeps the
//! workloads from influencing each other and lets the peak memory of a run belong to that
//! workload alone. The `report` benchmark drives all of them and lines the results up.
//!
//! Binaries accept these arguments.
//!
//! | Argument           | Effect                                                   |
//! | ------------------ | -------------------------------------------------------- |
//! | --list             | Print every workload as group, name and unit.            |
//! | --workload NAME    | Run only that workload and print one tab-separated row.  |
//! | --quick            | Do a quarter of the work.                                |
//!
//! A row holds the allocator, the workload, the value, the unit and the peak resident memory of
//! the process in MiB. Throughput is in millions of operations per second, where one operation is
//! an allocation together with its free unless the workload says otherwise.

mod churn;
mod concurrent;
mod harness;
mod memory;
mod micro;
mod resize;
mod structures;

use churn::{MEDIUM, MIXED, SMALL, Sizes};
use harness::{Outcome, Scale, peak_resident_mib};

type Run = fn(Scale) -> Outcome;

/// A named workload in a group.
struct Workload {
    group: &'static str,
    name: &'static str,
    run: Run,
}

const fn w(group: &'static str, name: &'static str, run: Run) -> Workload {
    Workload { group, name, run }
}

const SKEWED: Sizes = Sizes::Skewed;

/// Every workload, in the order the report prints them.
const WORKLOADS: &[Workload] = &[
    w("pair", "pair 16 B", micro::pair::<16>),
    w("pair", "pair 64 B", micro::pair::<64>),
    w("pair", "pair 256 B", micro::pair::<256>),
    w("pair", "pair 1 KiB", micro::pair::<1024>),
    w("pair", "pair 4 KiB", micro::pair::<4096>),
    w("pair", "pair 16 KiB", micro::pair::<16384>),
    w("pair", "pair 64 KiB", micro::pair::<65536>),
    w("pair", "pair 256 KiB", micro::pair::<262_144>),
    w("pair", "pair 1 MiB", micro::pair::<1_048_576>),
    w("bulk", "bulk 32 B in order", micro::bulk::<32, false>),
    w("bulk", "bulk 32 B shuffled", micro::bulk::<32, true>),
    w("bulk", "bulk 512 B shuffled", micro::bulk::<512, true>),
    w("churn", "churn small", |s| churn::churn(1, SMALL, s)),
    w("churn", "churn medium", |s| churn::churn(1, MEDIUM, s)),
    w("churn", "churn mixed", |s| churn::churn(1, MIXED, s)),
    w("churn", "churn skewed", |s| churn::churn(1, SKEWED, s)),
    w("churn", "churn mixed 4 threads", |s| churn::churn(4, MIXED, s)),
    w("churn", "churn mixed 8 threads", |s| churn::churn(8, MIXED, s)),
    w("churn", "churn mixed 16 threads", |s| churn::churn(16, MIXED, s)),
    w("churn", "churn skewed 8 threads", |s| churn::churn(8, SKEWED, s)),
    w("sharing", "larson 8 threads", concurrent::larson),
    w("sharing", "ring 4 threads", |s| concurrent::ring(4, s)),
    w("sharing", "ring 8 threads", |s| concurrent::ring(8, s)),
    w("sharing", "channel 1 pair", |s| concurrent::channel_pairs(1, s)),
    w("sharing", "channel 4 pairs", |s| concurrent::channel_pairs(4, s)),
    w("sharing", "shared table 8 threads", |s| concurrent::shared_table(8, s)),
    w("sharing", "thread lifecycle", concurrent::thread_lifecycle),
    w("false sharing", "cache scratch 8 threads", |s| concurrent::cache_scratch(8, s)),
    w("false sharing", "cache thrash 8 threads", |s| concurrent::cache_thrash(8, s)),
    w("collections", "hash map", structures::hash_map),
    w("collections", "btree map", structures::btree_map),
    w("collections", "strings", structures::strings),
    w("collections", "linked list", structures::linked_list),
    w("collections", "binary trees", structures::binary_trees),
    w("collections", "tree rewrite", structures::tree_rewrite),
    w("resizing", "vec growth", resize::vec_growth),
    w("resizing", "string growth", resize::string_growth),
    w("resizing", "many vectors", resize::many_vectors),
    w("resizing", "realloc random", micro::realloc_random),
    w("large", "medium blocks", churn::medium_blocks),
    w("large", "large blocks", churn::large_blocks),
    w("memory", "burst retention", memory::burst_retention),
    w("memory", "half freed", memory::half_freed),
];

/// Runs the workload that the arguments select, or lists them.
pub(crate) fn run(allocator: &str) {
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| {
        args.iter().position(|arg| arg == flag).and_then(|at| args.get(at + 1)).cloned()
    };
    if args.iter().any(|arg| arg == "--list") {
        for workload in WORKLOADS {
            let unit = if workload.group == "memory" { "MiB" } else { "Mops/s" };
            println!("{}\t{}\t{unit}", workload.group, workload.name);
        }
        return;
    }
    let scale = Scale::new(args.iter().any(|arg| arg == "--quick"));
    let wanted = value_of("--workload");
    for workload in WORKLOADS.iter().filter(|w| wanted.as_ref().is_none_or(|name| *name == w.name))
    {
        // The first run warms up the allocator and the page cache, and the second is measured.
        if workload.group != "memory" {
            let _ = (workload.run)(scale);
        }
        let (value, unit) = match (workload.run)(scale) {
            Outcome::Throughput { ops, elapsed } => {
                (ops as f64 / elapsed.as_secs_f64() / 1e6, "Mops/s")
            }
            Outcome::Footprint(mib) => (mib, "MiB"),
        };
        println!("{allocator}\t{}\t{value:.3}\t{unit}\t{:.1}", workload.name, peak_resident_mib());
    }
}
