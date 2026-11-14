//! The shape of `benchmarks.json`.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::Path};

/// Every recorded run, one per suite and machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Data {
    /// The runs, in the order the documents show them.
    pub runs: Vec<Run>,
}

/// The results of one suite on one machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// `dev`, `apps` or `mimalloc-bench`.
    pub suite: String,
    /// `macos` or `linux`.
    pub machine: String,
    /// The hardware and operating system, as people read it.
    pub description: String,
    /// The day the run finished, as YYYY-MM-DD.
    pub date: String,
    /// How many times every workload ran under every allocator.
    pub samples: usize,
    /// The allocator that comparisons are relative to.
    pub baseline: String,
    /// The allocators, in the order of the table columns.
    pub allocators: Vec<String>,
    /// The workloads, in the order of the table rows.
    pub workloads: Vec<Workload>,
}

/// One workload and its results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workload {
    /// The group the workload belongs to.
    pub group: String,
    /// The name of the workload.
    pub name: String,
    /// Whether a lower value of the warm, value and cold metrics is better.
    pub lower: bool,
    /// Whether the primary metric is an amount of memory.
    pub size: bool,
    /// The metric the tables and the summary use: warm, value, p99 or time.
    pub primary: String,
    /// The results by metric.
    pub metrics: BTreeMap<String, Metric>,
}

/// The results of every allocator on one metric of a workload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// The unit of the values.
    pub unit: String,
    /// The results by allocator.
    pub results: BTreeMap<String, Stat>,
}

/// The summary of the samples of one allocator on one metric.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    /// The median value.
    pub median: f64,
    /// The bounds of a 95% interval of the median.
    pub lo: f64,
    /// See [`lo`](Self::lo).
    pub hi: f64,
    /// The median of the peak resident memory of the runs, in MiB.
    pub peak_mib: f64,
}

/// The metrics where lower is better whatever the workload.
pub const LATENCY: [&str; 3] = ["p50", "p99", "p999"];

impl Workload {
    /// Whether a lower value of `metric` is better.
    pub fn lower_is_better(&self, metric: &str) -> bool {
        LATENCY.contains(&metric) || self.lower
    }

    /// The result of `allocator` on `metric`.
    pub fn stat(&self, metric: &str, allocator: &str) -> Option<&Stat> {
        self.metrics.get(metric)?.results.get(allocator)
    }

    /// The unit of `metric`, or of the primary metric if the workload has no such metric.
    pub fn unit(&self, metric: &str) -> &str {
        self.metrics.get(metric).or_else(|| self.metrics.get(&self.primary)).map_or("", |m| &m.unit)
    }

    /// Whether the workload measures speed, which is what the speed summary takes.
    pub fn is_speed(&self) -> bool {
        !self.size && self.primary != "p99"
    }
}

impl Run {
    /// The key that identifies the run in the file.
    pub fn key(&self) -> (&str, &str) {
        (&self.suite, &self.machine)
    }
}

/// The order in which suites and machines are shown.
fn rank(run: &Run) -> (usize, usize) {
    let suite = ["dev", "apps", "mimalloc-bench"].iter().position(|s| *s == run.suite);
    let machine = ["macos", "linux"].iter().position(|m| *m == run.machine);
    (suite.unwrap_or(9), machine.unwrap_or(9))
}

impl Data {
    /// Reads the file, or returns no runs if there is none.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Writes the file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        text.push('\n');
        fs::write(path, text)
    }

    /// Adds a run, replacing the one of the same suite and machine.
    pub fn record(&mut self, run: Run) {
        self.runs.retain(|old| old.key() != run.key());
        self.runs.push(run);
        self.runs.sort_by_key(rank);
    }

    /// The run of a suite on a machine.
    pub fn run(&self, suite: &str, machine: &str) -> Option<&Run> {
        self.runs.iter().find(|run| run.key() == (suite, machine))
    }
}

/// Rounds a value to four significant digits, which keeps the file small and its diffs readable.
pub fn round(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let scale = 10_f64.powi(3 - value.abs().log10().floor() as i32);
    (value * scale).round() / scale
}
