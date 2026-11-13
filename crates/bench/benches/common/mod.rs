//! The workloads shared by every allocator benchmark.
//!
//! A program has exactly one global allocator, so each allocator gets its own benchmark binary
//! that installs it and calls [`run`]. A binary runs one workload per invocation, which keeps the
//! workloads from influencing each other and lets the peak memory of a run belong to that
//! workload alone. The `report` benchmark drives all of them and lines the results up.
//!
//! Binaries accept these arguments.
//!
//! | Argument           | Effect                                                              |
//! | ------------------ | ------------------------------------------------------------------- |
//! | --list             | Print every workload as group, name, unit and better.              |
//! | --workload NAME    | Run only that workload and print its result lines.                  |
//! | --quick            | Do a quarter of the work.                                           |
//!
//! This is version 2 of the protocol. A line of --list has four tab-separated fields: the group,
//! the name, the unit of the workload and whether a higher or a lower value is better. A result
//! line has six: the allocator, the workload, the metric, the value, the unit and the peak
//! resident memory of the process in MiB at the end of the run. A workload prints these metrics.
//!
//! | Kind       | Groups   | Metrics             | Unit   | Better |
//! | ---------- | -------- | ------------------- | ------ | ------ |
//! | Throughput | most     | cold, warm          | Mops/s | higher |
//! | Memory     | memory   | value               | MiB    | lower  |
//! | Latency    | latency  | p50, p99, p999      | ns     | lower  |
//!
//! A throughput workload runs twice in the process. The cold metric is the first pass, which pays
//! for faulting in memory and for the allocator setting itself up, and the warm metric is the
//! second pass, which finds the allocator and the page cache in the state of a running program.
//! The primary metric of a workload is warm if it has one, then value, then p99. Throughput is in
//! millions of operations per second, where one operation is an allocation together with its free
//! unless the workload says otherwise.

mod churn;
mod concurrent;
mod harness;
mod latency;
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
    w("churn", "churn mixed oversubscribed", churn::churn_oversubscribed),
    w("churn", "churn large set", churn::churn_large_set),
    w("churn", "churn with work", churn::churn_with_work),
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
    w("collections", "binary trees with work", structures::binary_trees_with_work),
    w("resizing", "vec growth", resize::vec_growth),
    w("resizing", "string growth", resize::string_growth),
    w("resizing", "many vectors", resize::many_vectors),
    w("resizing", "realloc random", micro::realloc_random),
    w("large", "medium blocks", churn::medium_blocks),
    w("large", "large blocks", churn::large_blocks),
    w("memory", "burst retention", memory::burst_retention),
    w("memory", "half freed", memory::half_freed),
    w("memory", "phase churn", memory::phase_churn),
    w("latency", "latency churn 1 thread", latency::churn_1_thread),
    w("latency", "latency churn 8 threads", latency::churn_8_threads),
    w("latency", "latency remote frees", latency::remote_frees),
];

/// What a workload reports, which its group tells.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Throughput,
    Memory,
    Latency,
}

impl Kind {
    const fn of(group: &str) -> Self {
        match group.as_bytes() {
            b"memory" => Self::Memory,
            b"latency" => Self::Latency,
            _ => Self::Throughput,
        }
    }

    const fn unit(self) -> &'static str {
        match self {
            Self::Throughput => "Mops/s",
            Self::Memory => "MiB",
            Self::Latency => "ns",
        }
    }

    const fn better(self) -> &'static str {
        match self {
            Self::Throughput => "higher",
            Self::Memory | Self::Latency => "lower",
        }
    }

    /// The passes of a run. Only throughput is run twice, and its passes are named.
    const fn passes(self) -> &'static [&'static str] {
        match self {
            Self::Throughput => &["cold", "warm"],
            Self::Memory | Self::Latency => &["value"],
        }
    }
}

/// The metrics and values that an outcome holds. `pass` names the metric of a throughput.
fn metrics(outcome: Outcome, pass: &'static str) -> Vec<(&'static str, f64)> {
    match outcome {
        Outcome::Throughput { ops, elapsed } => {
            vec![(pass, ops as f64 / elapsed.as_secs_f64() / 1e6)]
        }
        Outcome::Footprint(mib) => vec![("value", mib)],
        Outcome::Latency { p50, p99, p999 } => vec![("p50", p50), ("p99", p99), ("p999", p999)],
    }
}

/// Runs the workload that the arguments select, or lists them.
pub(crate) fn run(allocator: &str) {
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| {
        args.iter().position(|arg| arg == flag).and_then(|at| args.get(at + 1)).cloned()
    };
    if args.iter().any(|arg| arg == "--list") {
        for workload in WORKLOADS {
            let kind = Kind::of(workload.group);
            println!("{}	{}	{}	{}", workload.group, workload.name, kind.unit(), kind.better());
        }
        return;
    }
    let scale = Scale::new(args.iter().any(|arg| arg == "--quick"));
    let wanted = value_of("--workload");
    let selected: Vec<_> =
        WORKLOADS.iter().filter(|w| wanted.as_ref().is_none_or(|name| *name == w.name)).collect();
    if selected.is_empty() {
        eprintln!("no workload named {}", wanted.unwrap_or_default());
        std::process::exit(2);
    }
    for workload in selected {
        let kind = Kind::of(workload.group);
        // The first pass of a throughput workload is reported as it is, cold. The second runs on
        // an allocator and a page cache that the first one warmed up.
        let rows: Vec<_> =
            kind.passes().iter().flat_map(|pass| metrics((workload.run)(scale), pass)).collect();
        // Every line carries the peak at the end of the run, which is the peak of the whole run.
        let peak = peak_resident_mib();
        for (metric, value) in rows {
            println!("{allocator}	{}	{metric}	{value:.3}	{}	{peak:.1}", workload.name, kind.unit());
        }
    }
}
