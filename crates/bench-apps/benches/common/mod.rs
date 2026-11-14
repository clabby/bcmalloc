//! The application workloads shared by every allocator benchmark.
//!
//! The workloads are programs written for their own purpose: a JSON pipeline, a regex engine, a
//! Rust parser, an HTTP server with its clients and a graph of async tasks. The allocator is never
//! tuned against them, so a result here shows whether a gain on the synthetic workloads of the main
//! crate carries over to ordinary code.
//!
//! A program has exactly one global allocator, so each allocator gets its own benchmark binary
//! that installs it and calls [`run`]. A binary runs one workload per invocation, which keeps the
//! workloads from influencing each other and lets the peak memory of a run belong to that
//! workload alone. The `report` benchmark of the main crate can drive these binaries too.
//!
//! Binaries speak protocol version 2 and accept these arguments.
//!
//! | Argument        | Effect                                                              |
//! | --------------- | ------------------------------------------------------------------- |
//! | --list          | Print one line per workload: group, name, unit and better.          |
//! | --workload NAME | Run only that workload and print its result lines.                  |
//! | --quick         | Do a quarter of the work.                                           |
//!
//! A result line holds the allocator, the workload, the metric, the value, the unit and the peak
//! resident memory of the process in MiB. Every workload here is a throughput workload, so it
//! prints two lines. The metric `cold` is the first pass in a fresh process and `warm` is a
//! second, measured pass. Higher is better for both. `http many` also prints the p99 latency of
//! a request in its warm pass, in microseconds, where lower is better.

mod channels;
mod http;
mod http_many;
mod json;
mod kv;
mod parallel;
mod pattern;
mod rss;
mod source;
mod text;

use std::time::{Duration, Instant};

/// How much work a workload does.
#[derive(Clone, Copy)]
pub(crate) struct Scale {
    quick: bool,
}

impl Scale {
    /// `base` units of work, or a quarter as many for a quick run.
    pub(crate) const fn count(self, base: usize) -> usize {
        if self.quick { base / 4 } else { base }
    }
}

/// What one pass of a workload did: an amount in the workload's unit, and the time it took.
pub(crate) struct Pass {
    amount: f64,
    elapsed: Duration,
    p99_us: Option<f64>,
}

impl Pass {
    /// A pass that did `amount` units of work since `start`.
    pub(crate) fn since(start: Instant, amount: f64) -> Self {
        Self { amount, elapsed: start.elapsed(), p99_us: None }
    }

    fn rate(&self) -> f64 {
        self.amount / self.elapsed.as_secs_f64()
    }
}

/// Runs `pass` twice. The first pass is the cold one and the second is the measured one.
fn twice(mut pass: impl FnMut() -> Pass) -> [Pass; 2] {
    let cold = pass();
    [cold, pass()]
}

/// A named workload.
struct Workload {
    name: &'static str,
    unit: &'static str,
    /// Prepares the input outside the clock, then runs the cold and the warm pass.
    run: fn(Scale) -> [Pass; 2],
}

const GROUP: &str = "apps";

/// Every workload, in the order the report prints them.
const WORKLOADS: &[Workload] = &[
    Workload { name: "json", unit: "MB/s", run: json::run },
    Workload { name: "regex", unit: "MB/s", run: pattern::run },
    Workload { name: "syn", unit: "MB/s", run: source::run },
    Workload { name: "http", unit: "kreq/s", run: http::run },
    Workload { name: "tokio channels", unit: "Mmsg/s", run: channels::run },
    Workload { name: "kv store", unit: "ops/s", run: kv::run },
    Workload { name: "http many", unit: "req/s", run: http_many::run },
    Workload { name: "rayon", unit: "records/s", run: parallel::run },
];

/// Runs the workloads that the arguments select, or lists them.
pub(crate) fn run(allocator: &str) {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--list") {
        for workload in WORKLOADS {
            println!("{GROUP}\t{}\t{}\thigher", workload.name, workload.unit);
        }
        return;
    }
    let scale = Scale { quick: args.iter().any(|arg| arg == "--quick") };
    let wanted = args.iter().position(|arg| arg == "--workload").and_then(|at| args.get(at + 1));
    if let Some(name) = wanted &&
        !WORKLOADS.iter().any(|workload| workload.name == name)
    {
        eprintln!("unknown workload {name:?}");
        std::process::exit(2);
    }
    for workload in WORKLOADS.iter().filter(|w| wanted.is_none_or(|name| name == w.name)) {
        let [cold, warm] = (workload.run)(scale);
        // The peak covers the whole run, so both lines carry the same figure.
        let (name, unit, peak) = (workload.name, workload.unit, rss::peak_resident_mib());
        let p99 = warm.p99_us;
        for (metric, pass) in [("cold", cold), ("warm", warm)] {
            println!("{allocator}\t{name}\t{metric}\t{:.3}\t{unit}\t{peak:.1}", pass.rate());
        }
        if let Some(p99) = p99 {
            println!("{allocator}\t{name}\tp99\t{p99:.3}\tus\t{peak:.1}");
        }
    }
}
