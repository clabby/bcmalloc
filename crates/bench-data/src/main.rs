//! Renders the benchmark documents from `benchmarks.json`, checks that they are up to date, and
//! imports the results of mimalloc-bench.
//!
//! ```text
//! bench-data render                          write BENCHMARKS.md, README.md and the figure
//! bench-data check                           fail if any of them is out of date
//! bench-data import-mimalloc-bench CSV       record a mimalloc-bench run from its benchres.csv
//! ```

use bcmalloc_bench_data::{Data, Metric, Run, Stat, Workload, docs, machine, model::round};
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

/// The root of the repository, two levels above this crate.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The names of the allocators as mimalloc-bench abbreviates them.
fn allocator_name(short: &str) -> String {
    match short {
        "bc" => "bcmalloc",
        "mi" => "mimalloc",
        "sn" => "snmalloc",
        "je" => "jemalloc",
        "tc" => "tcmalloc",
        "sys" => "system",
        other => other,
    }
    .to_string()
}

/// Parses a time as mimalloc-bench writes it, in seconds or as minutes:seconds.
fn seconds(text: &str) -> Option<f64> {
    match text.split_once(':') {
        Some((minutes, rest)) => {
            Some(minutes.parse::<f64>().ok()? * 60.0 + rest.parse::<f64>().ok()?)
        }
        None => text.parse().ok(),
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) / 2.0 }
}

/// Reads a benchres.csv: one line per program, allocator and run, with the time in seconds and
/// the peak resident memory in KiB. The interval of a median is the range of the runs.
fn import(csv: &str) -> Run {
    let mut samples: BTreeMap<(String, String), Vec<(f64, f64)>> = BTreeMap::new();
    let mut programs: Vec<String> = Vec::new();
    let mut allocators: Vec<String> = Vec::new();
    for line in csv.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [program, allocator, time, rss, ..] = fields[..] else { continue };
        let (Some(time), Ok(rss)) = (seconds(time), rss.parse::<f64>()) else { continue };
        let allocator = allocator_name(allocator);
        if !programs.iter().any(|p| p == program) {
            programs.push(program.to_string());
        }
        if !allocators.contains(&allocator) {
            allocators.push(allocator.clone());
        }
        samples.entry((program.to_string(), allocator)).or_default().push((time, rss / 1024.0));
    }
    let order = ["bcmalloc", "mimalloc", "snmalloc", "jemalloc", "tcmalloc", "system"];
    allocators.sort_by_key(|a| order.iter().position(|o| o == a).unwrap_or(order.len()));
    let mut runs = 0;
    let workloads = programs
        .iter()
        .map(|program| {
            let mut results = BTreeMap::new();
            for allocator in &allocators {
                let Some(found) = samples.get(&(program.clone(), allocator.clone())) else {
                    continue
                };
                runs = runs.max(found.len());
                let mut times: Vec<f64> = found.iter().map(|s| s.0).collect();
                let mut peaks: Vec<f64> = found.iter().map(|s| s.1).collect();
                let lo = times.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = times.iter().copied().fold(0.0, f64::max);
                let stat = Stat {
                    median: round(median(&mut times)),
                    lo: round(lo),
                    hi: round(hi),
                    peak_mib: round(median(&mut peaks)),
                };
                results.insert(allocator.clone(), stat);
            }
            let metric = Metric { unit: "s".to_string(), results };
            Workload {
                group: "mimalloc-bench".to_string(),
                name: program.clone(),
                lower: true,
                size: false,
                primary: "time".to_string(),
                metrics: BTreeMap::from([("time".to_string(), metric)]),
            }
        })
        .collect();
    Run {
        suite: "mimalloc-bench".to_string(),
        machine: machine::key().to_string(),
        description: machine::describe(),
        date: machine::today(),
        samples: runs,
        baseline: "mimalloc".to_string(),
        allocators,
        workloads,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = root();
    let path = root.join("benchmarks.json");
    let fail = |message: String| {
        eprintln!("{message}");
        ExitCode::FAILURE
    };
    let mut data = match Data::load(&path) {
        Ok(data) => data,
        Err(error) => return fail(format!("cannot read {}: {error}", path.display())),
    };
    match args.first().map(String::as_str) {
        Some("render") => {}
        Some("check") => {
            return match docs::stale(&root, &data) {
                Ok(stale) if stale.is_empty() => ExitCode::SUCCESS,
                Ok(stale) => {
                    for path in stale {
                        eprintln!("{} is out of date, run: just bench-render", path.display());
                    }
                    ExitCode::FAILURE
                }
                Err(error) => fail(format!("cannot render: {error}")),
            };
        }
        Some("import-mimalloc-bench") => {
            let Some(csv) = args.get(1) else { return fail("which benchres.csv?".to_string()) };
            match std::fs::read_to_string(csv) {
                Ok(text) => data.record(import(&text)),
                Err(error) => return fail(format!("cannot read {csv}: {error}")),
            }
            if let Err(error) = data.save(&path) {
                return fail(format!("cannot write {}: {error}", path.display()));
            }
        }
        _ => {
            return fail("usage: bench-data render | check | import-mimalloc-bench CSV".to_string())
        }
    }
    match docs::write(&root, &data) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(format!("cannot render: {error}")),
    }
}
