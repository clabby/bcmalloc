//! Runs every workload under every allocator and prints the comparison as Markdown.
//!
//! Usage is  cargo bench --bench report  with these options after two dashes.
//!
//! | Option             | Effect                                                                |
//! | ------------------ | --------------------------------------------------------------------- |
//! | --quick            | Do a quarter of the work in every workload.                           |
//! | --runs N           | Run each workload N times per allocator. Default 10.                  |
//! | --rounds N         | The same as --runs.                                                   |
//! | --filter A,B       | Run only workloads whose name or group contains one of the texts.     |
//! | --only A,B         | Run only the named allocators. With --compare it picks the rows.      |
//! | --baseline A       | The allocator that the summary compares against. Default mimalloc.    |
//! | --perf             | Count hardware events of every run with perf stat (Linux).            |
//! | --cpus LIST        | Run every child under taskset -c LIST (Linux), for instance 0-3.      |
//! | --manifest PATH    | Build and drive the benches of another Cargo.toml than this crate's.  |
//! | --save PATH        | Also copy the raw samples to PATH.                                    |
//! | --compare OLD NEW  | Compare two saved sample files. Runs no benchmarks. Shows bcmalloc.   |
//! | --load PATH        | Print the report of a saved sample file instead of running.           |
//! | --self-test        | Check the statistics helpers and exit.                                |
//!
//! Each run is a fresh process, so no workload sees the heap that an earlier one left behind. The
//! allocators take turns within every run, in a rotating order, so that drift in the machine hits
//! all of them alike. Every child gets an environment variable BCMALLOC_BENCH_PAD of random length
//! from a seeded generator. It moves the initial stack from run to run, so that one lucky or
//! unlucky alignment does not decide a result (Mytkowicz et al., ASPLOS 2009).
//!
//! The tables show the median of the runs. A 95% interval of each median comes from a bootstrap
//! with a fixed seed. The best median in a row is bold, and so is every median whose interval
//! overlaps the best one's, because the two cannot be told apart. Throughput workloads also get a
//! table of the cold pass, which is the first pass in a fresh process, and latency workloads a
//! table of p50, p99 and p999. The summary uses the primary metric of each workload, which is
//! warm, else value, else p99.
//!
//! The raw samples go to target/bench.tsv, one line per sample with a header. With --compare the
//! median change of each allocator, workload and metric is printed with a 95% bootstrap interval
//! of the ratio of the medians, and a change whose interval excludes zero is marked.
//!
//! With --perf each run is wrapped in perf stat, and the counters are printed after its result.
//! They cover the whole process, which includes the warm-up run of the workload. Set
//! BCMALLOC_PERF to the command to use, for instance "sudo perf", when perf needs privileges.
//!
//! A benchmark binary speaks version 2 of the protocol. It lists workloads as group, name, unit
//! and better, and it prints result lines of allocator, workload, metric, value, unit and peak
//! resident MiB. Version 1 lines have no metric and no better. They count as the metric warm,
//! and lower is better only for the unit MiB.

use bcmalloc_bench_data::{self as data, Data, Run, docs, machine, markdown, model::round};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The order in which known allocators are shown. Others follow alphabetically.
const ALLOCATORS: [&str; 5] = ["system", "bcmalloc", "mimalloc", "jemalloc", "snmalloc"];

/// The metrics that can stand for a workload, in order of preference.
const PRIMARY: [&str; 3] = ["warm", "value", "p99"];

/// The metrics of a latency workload.
const LATENCY: [&str; 3] = ["p50", "p99", "p999"];

const TSV_HEADER: &str = "allocator\tworkload\tmetric\trun\tvalue\tunit\tpeak_mib";

/// The bootstrap draws this many resamples. The seed is fixed, so a table is reproducible.
const RESAMPLES: usize = 2000;
const BOOTSTRAP_SEED: u64 = 0xB007_57A9;
const PAD_SEED: u64 = 0x5EED_CAFE;

/// The longest random environment padding, exclusive.
const PAD_LIMIT: usize = 4096;

/// The samples of one allocator on one workload and metric.
#[derive(Default)]
struct Series {
    unit: String,
    runs: Vec<usize>,
    values: Vec<f64>,
    peaks: Vec<f64>,
}

/// Samples by allocator, workload and metric.
type Results = BTreeMap<(String, String, String), Series>;

struct Workload {
    group: String,
    name: String,
    unit: String,
    /// Whether a lower value is better for the throughput, value and cold metrics.
    lower: bool,
    /// The metric that the tables and the summary use. Set once the samples are in.
    primary: String,
}

/// One result line of a benchmark binary.
#[derive(Debug, PartialEq)]
struct Line {
    allocator: String,
    workload: String,
    metric: String,
    value: f64,
    unit: String,
    peak_mib: f64,
}

/// A small xorshift generator. The benches cannot use rand.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value in 0..n. The low bits of xorshift are weak, so use the high ones.
    const fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 11) % n as u64) as usize
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| {
        args.iter().position(|arg| arg == flag).and_then(|at| args.get(at + 1)).cloned()
    };
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let only = value_of("--only");
    let filter = value_of("--filter");

    if has("--self-test") {
        self_test();
        return;
    }
    if let Some(at) = args.iter().position(|arg| arg == "--compare") {
        let (Some(old), Some(new)) = (args.get(at + 1), args.get(at + 2)) else {
            eprintln!("--compare needs two files, OLD.tsv and NEW.tsv");
            std::process::exit(2);
        };
        let only = only.unwrap_or_else(|| "bcmalloc".to_string());
        compare(old, new, &only, filter.as_deref());
        return;
    }

    let runs: usize = value_of("--runs")
        .or_else(|| value_of("--rounds"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
        .max(1);
    let baseline = value_of("--baseline").unwrap_or_else(|| "mimalloc".to_string());
    let mut driver = Driver {
        quick: has("--quick"),
        perf: has("--perf"),
        cpus: value_of("--cpus").filter(|list| taskset_available(list)),
        rng: Rng::new(PAD_SEED),
    };

    let binaries = build(value_of("--manifest").as_deref());
    let allocators: Vec<String> = ordered(&binaries)
        .into_iter()
        .filter(|name| only.as_ref().is_none_or(|list| list.split(',').any(|item| item == name)))
        .collect();
    if allocators.is_empty() {
        eprintln!("no benchmark binary matches");
        std::process::exit(2);
    }
    let mut workloads = list(&binaries[&allocators[0]], filter.as_deref());

    let loaded = value_of("--load");
    let mut results = loaded.as_deref().map_or_else(Results::new, read_tsv);
    let saved_runs =
        results.values().flat_map(|series| series.runs.iter()).max().map(|run| run + 1);
    let runs = if loaded.is_some() { saved_runs.unwrap_or(0) } else { runs };
    for run in (0..runs).filter(|_| loaded.is_none()) {
        for workload in &workloads {
            for turn in 0..allocators.len() {
                let allocator = &allocators[(turn + run) % allocators.len()];
                let Some((lines, counters)) =
                    driver.run(&binaries[allocator], allocator, &workload.name)
                else {
                    continue;
                };
                for line in &lines {
                    eprintln!(
                        "run {run} {allocator:>9} {:<26} {:>6} {:>12.2} {}{counters}",
                        workload.name, line.metric, line.value, line.unit
                    );
                    let key = (allocator.clone(), line.workload.clone(), line.metric.clone());
                    let series = results.entry(key).or_default();
                    series.unit.clone_from(&line.unit);
                    series.runs.push(run);
                    series.values.push(line.value);
                    series.peaks.push(line.peak_mib);
                }
            }
        }
    }
    choose_primary(&mut workloads, &results);

    let tsv = to_tsv(&results);
    let _ = fs::create_dir_all(workspace().join("target"));
    let _ = fs::write(workspace().join("target/bench.tsv"), &tsv);
    if let Some(path) = value_of("--save") &&
        let Err(error) = fs::write(workspace().join(&path), &tsv)
    {
        eprintln!("cannot save to {path}: {error}");
    }

    let run = to_run(&workloads, &allocators, &results, runs, &baseline);
    print!("{}", markdown::report(&run));

    // A run of every workload under every allocator is the record of this machine. A partial or
    // quick run is not.
    let complete = filter.is_none() && only.is_none() && !driver.quick && !has("--no-record");
    if complete {
        let manifest = value_of("--manifest");
        let suite = if manifest.is_some_and(|m| m.contains("bench-apps")) { "apps" } else { "dev" };
        if let Err(error) = record(suite, run) {
            eprintln!("cannot record the results in benchmarks.json: {error}");
        }
    }
}

/// Turns the samples into a run of benchmarks.json, with the median and its interval of every
/// series.
fn to_run(
    workloads: &[Workload],
    allocators: &[String],
    results: &Results,
    samples: usize,
    baseline: &str,
) -> Run {
    let workloads = workloads
        .iter()
        .map(|workload| {
            let mut metrics: BTreeMap<String, data::Metric> = BTreeMap::new();
            for ((allocator, name, metric), series) in results {
                if *name != workload.name || series.values.is_empty() {
                    continue;
                }
                let (lo, hi) = median_interval(&series.values);
                let stat = data::Stat {
                    median: round(median(&series.values)),
                    lo: round(lo),
                    hi: round(hi),
                    peak_mib: round(median(&series.peaks)),
                };
                metrics
                    .entry(metric.clone())
                    .or_insert_with(|| data::Metric {
                        unit: series.unit.clone(),
                        results: BTreeMap::new(),
                    })
                    .results
                    .insert(allocator.clone(), stat);
            }
            data::Workload {
                group: workload.group.clone(),
                name: workload.name.clone(),
                lower: workload.lower,
                size: workload.lower && workload.primary != "p99",
                primary: workload.primary.clone(),
                metrics,
            }
        })
        .collect();
    Run {
        suite: String::new(),
        machine: machine::key().to_string(),
        description: machine::describe(),
        date: machine::today(),
        samples,
        baseline: baseline.to_string(),
        allocators: allocators.to_vec(),
        workloads,
    }
}

/// Records a run in benchmarks.json and renders the documents from it again.
fn record(suite: &str, mut run: Run) -> std::io::Result<()> {
    run.suite = suite.to_string();
    let path = workspace().join("benchmarks.json");
    let mut data = Data::load(&path)?;
    data.record(run);
    data.save(&path)?;
    docs::write(&workspace(), &data)?;
    eprintln!("recorded in benchmarks.json, and rendered BENCHMARKS.md, README.md and the figure");
    Ok(())
}

// ---- Running the benchmarks ----

/// How the children are started.
struct Driver {
    quick: bool,
    perf: bool,
    cpus: Option<String>,
    rng: Rng,
}

impl Driver {
    /// Runs one workload in a fresh process. Returns its result lines and, with perf, a line of
    /// counters.
    fn run(
        &mut self,
        binary: &str,
        allocator: &str,
        workload: &str,
    ) -> Option<(Vec<Line>, String)> {
        let mut words: Vec<String> = Vec::new();
        if let Some(cpus) = &self.cpus {
            words.extend(["taskset".to_string(), "-c".to_string(), cpus.clone()]);
        }
        if self.perf {
            let launcher = std::env::var("BCMALLOC_PERF").unwrap_or_else(|_| "perf".to_string());
            words.extend(launcher.split_whitespace().map(str::to_string));
            words.extend(["stat", "-x", ",", "-e", PERF_EVENTS, "--"].map(str::to_string));
        }
        words.push(binary.to_string());
        words.extend(["--workload".to_string(), workload.to_string()]);
        if self.quick {
            words.push("--quick".to_string());
        }
        let mut command = Command::new(&words[0]);
        command.args(&words[1..]);
        command.env("BCMALLOC_BENCH_PAD", "x".repeat(self.rng.below(PAD_LIMIT)));

        let output = match command.output() {
            Ok(output) => output,
            Err(error) => {
                eprintln!("{allocator} {workload}: cannot start {}: {error}", words[0]);
                return None;
            }
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<Line> = text
            .lines()
            .filter_map(parse_result)
            .filter(|line| line.workload == workload)
            .collect();
        if lines.is_empty() {
            eprintln!("{allocator} {workload}: no result ({})", output.status);
            return None;
        }
        let mut counters = String::new();
        if self.perf {
            // perf stat -x writes value,unit,event,... for every event.
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                let fields: Vec<_> = line.split(',').collect();
                if let [value, _, event, ..] = fields[..] {
                    counters += &format!("  {event}={value}");
                }
            }
        }
        Some((lines, counters))
    }
}

/// The events that --perf counts.
const PERF_EVENTS: &str =
    "task-clock,cycles,instructions,page-faults,L1-dcache-load-misses,dTLB-load-misses";

/// Whether --cpus can be honoured. It needs Linux and taskset.
fn taskset_available(list: &str) -> bool {
    if !cfg!(target_os = "linux") {
        eprintln!("warning: --cpus {list} is ignored, because taskset is a Linux tool");
        return false;
    }
    let found = Command::new("taskset").arg("--version").output().is_ok();
    if !found {
        eprintln!("warning: --cpus {list} is ignored, because taskset is not installed");
    }
    found
}

/// Builds every benchmark binary of a manifest, or of this crate, and returns the path of each by
/// name. The report itself is left out.
fn build(manifest: Option<&str>) -> BTreeMap<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command.args(["bench", "--no-run", "--message-format=json", "--manifest-path"]);
    // A relative manifest path is taken from the root of the workspace, where the recipes run.
    command.arg(manifest.map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        |path| workspace().join(path),
    ));
    let output = command.stderr(Stdio::inherit()).output().expect("cargo runs");
    let mut binaries = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.contains("\"kind\":[\"bench\"]") {
            continue;
        }
        let (Some(name), Some(path)) =
            (field_after(line, "\"name\":\""), field_after(line, "\"executable\":\""))
        else {
            continue;
        };
        if name != "report" {
            binaries.insert(name, path);
        }
    }
    binaries
}

/// The root of the workspace, two levels above this crate. Paths given on the command line are
/// taken from there, because the driver runs in this crate's directory. Absolute paths stay as
/// they are, since joining an absolute path replaces the base.
fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The binary names with the known allocators first, in their usual order.
fn ordered(binaries: &BTreeMap<String, String>) -> Vec<String> {
    let known = ALLOCATORS.iter().filter(|name| binaries.contains_key(**name));
    let others = binaries.keys().filter(|name| !ALLOCATORS.contains(&name.as_str()));
    known.map(|name| name.to_string()).chain(others.cloned()).collect()
}

/// The text between the first occurrence of `key` and the next quote.
fn field_after(line: &str, key: &str) -> Option<String> {
    let rest = &line[line.find(key)? + key.len()..];
    Some(rest[..rest.find('"')?].to_string())
}

fn list(binary: &str, filter: Option<&str>) -> Vec<Workload> {
    let output = Command::new(binary).arg("--list").output().expect("benchmark lists workloads");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_listing)
        .filter(|w| {
            filter.is_none_or(|texts| {
                texts.split(',').any(|text| w.name.contains(text) || w.group.contains(text))
            })
        })
        .collect()
}

/// Whether a lower value is better for a unit, for the files that do not say so.
fn lower_by_unit(unit: &str) -> bool {
    matches!(unit, "B" | "KiB" | "MiB" | "GiB" | "ns" | "us" | "ms" | "s")
}

/// Parses a line of --list. Version 1 has three fields and version 2 has four.
fn parse_listing(line: &str) -> Option<Workload> {
    let fields: Vec<&str> = line.split('\t').collect();
    let (group, name, unit, better) = match fields[..] {
        [group, name, unit] => (group, name, unit, None),
        [group, name, unit, better] => (group, name, unit, Some(better)),
        _ => return None,
    };
    let lower = better.map_or_else(|| lower_by_unit(unit), |better| better == "lower");
    Some(Workload {
        group: group.to_string(),
        name: name.to_string(),
        unit: unit.to_string(),
        lower,
        primary: String::new(),
    })
}

/// Parses a result line. Version 1 has five fields and no metric, which counts as warm.
fn parse_result(line: &str) -> Option<Line> {
    let fields: Vec<&str> = line.split('\t').collect();
    let (allocator, workload, metric, value, unit, peak) = match fields[..] {
        [a, w, m, v, u, p] => (a, w, m, v, u, p),
        [a, w, v, u, p] => (a, w, "warm", v, u, p),
        _ => return None,
    };
    Some(Line {
        allocator: allocator.to_string(),
        workload: workload.to_string(),
        metric: metric.to_string(),
        value: value.parse().ok()?,
        unit: unit.to_string(),
        peak_mib: peak.parse().ok()?,
    })
}

/// Picks the primary metric of every workload from the samples. A workload without samples keeps
/// the first choice.
fn choose_primary(workloads: &mut [Workload], results: &Results) {
    for workload in workloads {
        let has = |metric: &str| {
            results.keys().any(|(_, name, found)| *name == workload.name && found == metric)
        };
        workload.primary =
            PRIMARY.iter().find(|metric| has(metric)).unwrap_or(&PRIMARY[0]).to_string();
        let unit = results.iter().find_map(|((_, name, metric), series)| {
            (*name == workload.name && *metric == workload.primary).then_some(&series.unit)
        });
        if let Some(unit) = unit {
            workload.unit.clone_from(unit);
        }
    }
}

// ---- The samples file ----

fn to_tsv(results: &Results) -> String {
    let mut tsv = format!("{TSV_HEADER}\n");
    for ((allocator, workload, metric), series) in results {
        for ((run, value), peak) in series.runs.iter().zip(&series.values).zip(&series.peaks) {
            tsv += &format!(
                "{allocator}\t{workload}\t{metric}\t{run}\t{value:.3}\t{}\t{peak:.1}\n",
                series.unit
            );
        }
    }
    tsv
}

/// Reads a samples file. The old files have five fields and no run, which count as warm.
fn read_tsv(path: &str) -> Results {
    let text = fs::read_to_string(workspace().join(path)).unwrap_or_else(|error| {
        eprintln!("cannot read {path}: {error}");
        std::process::exit(2);
    });
    let mut results = Results::new();
    for line in text.lines().skip_while(|line| line.starts_with("allocator\t")) {
        let fields: Vec<&str> = line.split('\t').collect();
        let (allocator, workload, metric, run, value, unit, peak) = match fields[..] {
            [a, w, m, r, v, u, p] => (a, w, m, r.parse().unwrap_or(0), v, u, p),
            [a, w, v, u, p] => (a, w, "warm", 0, v, u, p),
            _ => continue,
        };
        let (Ok(value), Ok(peak)) = (value.parse(), peak.parse()) else { continue };
        let key = (allocator.to_string(), workload.to_string(), metric.to_string());
        let series = results.entry(key).or_default();
        series.unit = unit.to_string();
        series.runs.push(run);
        series.values.push(value);
        series.peaks.push(peak);
    }
    results
}

// ---- Statistics ----

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    median_sorted(&sorted)
}

fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    match n {
        0 => f64::NAN,
        _ if n % 2 == 1 => sorted[n / 2],
        _ => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

/// The median of a resample of `values` that is drawn with replacement.
fn resampled_median(values: &[f64], rng: &mut Rng, scratch: &mut Vec<f64>) -> f64 {
    scratch.clear();
    scratch.extend((0..values.len()).map(|_| values[rng.below(values.len())]));
    scratch.sort_by(f64::total_cmp);
    median_sorted(scratch)
}

/// The 2.5% and 97.5% points of the sorted values.
fn central_95(sorted: &[f64]) -> Option<(f64, f64)> {
    let last = sorted.len().checked_sub(1)?;
    let at = |p: f64| sorted[(p * last as f64).round() as usize];
    Some((at(0.025), at(0.975)))
}

/// A 95% interval of the median from a bootstrap with a fixed seed.
fn median_interval(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let mut rng = Rng::new(BOOTSTRAP_SEED);
    let mut scratch = Vec::with_capacity(values.len());
    let mut medians: Vec<f64> =
        (0..RESAMPLES).map(|_| resampled_median(values, &mut rng, &mut scratch)).collect();
    medians.sort_by(f64::total_cmp);
    central_95(&medians).expect("resamples exist")
}

/// A 95% interval of the ratio of the median of `new` to the median of `old`, from a bootstrap
/// with a fixed seed that resamples both sides. None when the ratio is not defined.
fn ratio_interval(old: &[f64], new: &[f64]) -> Option<(f64, f64)> {
    let mut rng = Rng::new(BOOTSTRAP_SEED);
    let mut scratch = Vec::new();
    let mut ratios: Vec<f64> = (0..RESAMPLES)
        .map(|_| {
            let old = resampled_median(old, &mut rng, &mut scratch);
            resampled_median(new, &mut rng, &mut scratch) / old
        })
        .filter(|ratio| ratio.is_finite())
        .collect();
    ratios.sort_by(f64::total_cmp);
    central_95(&ratios)
}

// ---- Comparing two sample files ----

/// Prints the change of the median from the old file to the new one for every allocator,
/// workload and metric that both have, and marks the changes that the bootstrap interval of the
/// ratio of medians separates from zero.
fn compare(old: &str, new: &str, only: &str, filter: Option<&str>) {
    let (old, new) = (read_tsv(old), read_tsv(new));
    let mut rows = vec![
        ["Allocator", "Workload", "Metric", "Old", "New", "Change", "95% interval", "Verdict"]
            .map(str::to_string)
            .to_vec(),
    ];
    for (key, new_series) in &new {
        let (allocator, workload, metric) = key;
        let Some(old_series) = old.get(key) else { continue };
        if !only.split(',').any(|item| item == allocator) ||
            filter.is_some_and(|texts| !texts.split(',').any(|text| workload.contains(text)))
        {
            continue;
        }
        let (old_median, new_median) = (median(&old_series.values), median(&new_series.values));
        let percent = |ratio: f64| (ratio - 1.0) * 100.0;
        let (change, interval, verdict) =
            match ratio_interval(&old_series.values, &new_series.values) {
                Some((lo, hi)) if old_median != 0.0 => {
                    let significant = lo > 1.0 || hi < 1.0;
                    let lower_is_better =
                        LATENCY.contains(&metric.as_str()) || lower_by_unit(&new_series.unit);
                    let verdict = match (significant, (new_median < old_median) == lower_is_better)
                    {
                        (false, _) => "",
                        (true, true) => "better",
                        (true, false) => "worse",
                    };
                    (
                        format!("{:+.1}%", percent(new_median / old_median)),
                        format!("{:+.1}% to {:+.1}%", percent(lo), percent(hi)),
                        verdict,
                    )
                }
                _ => ("n/a".to_string(), "n/a".to_string(), ""),
            };
        rows.push(vec![
            allocator.clone(),
            workload.clone(),
            metric.clone(),
            format!("{} ({})", markdown::fmt_value(old_median), old_series.values.len()),
            format!("{} ({})", markdown::fmt_value(new_median), new_series.values.len()),
            change,
            interval,
            verdict.to_string(),
        ]);
    }
    if rows.len() == 1 {
        eprintln!("no allocator, workload and metric in both files matched");
        return;
    }
    println!("Median change from the old file to the new one, with the sample count in brackets.");
    println!("A verdict means the 95% bootstrap interval of the ratio of medians excludes zero.\n");
    print!("{}", markdown::table(&rows));
}

// ---- Self-check ----

/// Checks the helpers. The report is a bench with no harness, so this stands in for unit tests.
fn self_test() {
    assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    assert!(median(&[]).is_nan());

    let noisy: Vec<f64> = (0..10).map(|i| 100.0 + f64::from(i % 5)).collect();
    let (lo, hi) = median_interval(&noisy);
    let middle = median(&noisy);
    assert!(lo <= middle && middle <= hi, "{lo} {middle} {hi}");
    assert!((100.0..=104.0).contains(&lo) && (100.0..=104.0).contains(&hi));
    assert_eq!(median_interval(&noisy), (lo, hi), "the bootstrap is deterministic");
    assert_eq!(median_interval(&[7.0; 6]), (7.0, 7.0));

    let doubled: Vec<f64> = noisy.iter().map(|v| v * 2.0).collect();
    let (lo, hi) = ratio_interval(&noisy, &noisy).unwrap();
    assert!(lo <= 1.0 && 1.0 <= hi, "same data must not differ: {lo} {hi}");
    let (lo, hi) = ratio_interval(&noisy, &doubled).unwrap();
    assert!(lo > 1.9 && hi < 2.1, "{lo} {hi}");
    assert!(ratio_interval(&[0.0; 4], &noisy).is_none());

    let cells = [Some((10.0, 9.0, 11.0)), Some((10.5, 10.0, 12.0)), Some((20.0, 19.0, 21.0)), None];
    assert_eq!(markdown::leaders(&cells, false), [false, false, true, false]);
    assert_eq!(markdown::leaders(&cells, true), [true, true, false, false]);
    let tied = [Some((20.0, 10.5, 21.0)), Some((20.5, 19.0, 21.0)), Some((9.0, 8.0, 10.0))];
    assert_eq!(markdown::leaders(&tied, false), [true, true, false]);

    let (v1, v2) = (parse_result("a\tw\t1.5\tMops/s\t3.0"), parse_result("a\tw\tp99\t80\tns\t4.5"));
    assert_eq!(v1.unwrap().metric, "warm");
    let v2 = v2.unwrap();
    assert_eq!((v2.metric.as_str(), v2.value, v2.peak_mib), ("p99", 80.0, 4.5));
    assert!(parse_result("not a result").is_none());
    assert!(parse_listing("g\tn\tMiB").unwrap().lower);
    assert!(!parse_listing("g\tn\tMops/s").unwrap().lower);
    assert!(parse_listing("g\tn\tMops/s\tlower").unwrap().lower);
    assert!(!parse_listing("g\tn\tMiB\thigher").unwrap().lower);

    let mut rng = Rng::new(PAD_SEED);
    let pads: Vec<usize> = (0..1000).map(|_| rng.below(PAD_LIMIT)).collect();
    assert!(pads.iter().all(|pad| *pad < PAD_LIMIT));
    assert!(pads.iter().any(|pad| *pad > 3000) && pads.iter().any(|pad| *pad < 1000));
    println!("self-test ok");
}
