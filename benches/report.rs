//! Runs every workload under every allocator and prints the comparison as Markdown.
//!
//! Usage is  cargo bench --bench report with these options after two dashes.
//!
//! | Option           | Effect                                                                  |
//! | ---------------- | ----------------------------------------------------------------------- |
//! | --quick          | Do a quarter of the work in every workload.                             |
//! | --rounds N       | Run each workload N times per allocator and keep the best. Default 3.   |
//! | --filter A,B     | Run only workloads whose name or group contains one of the texts.       |
//! | --only A,B       | Run only the named allocators.                                          |
//! | --baseline A     | The allocator that the summary compares against. Default mimalloc.      |
//! | --perf           | Count hardware events of every run with perf stat (Linux).              |
//!
//! Each run is a fresh process, so no workload sees the heap that an earlier one left behind. The
//! allocators take turns within every round, in a rotating order, so that drift in the machine
//! hits all of them alike. The raw rows go to target/bench.tsv.
//!
//! With --perf each run is wrapped in perf stat, and the counters are printed after its result.
//! They cover the whole process, which includes the warm-up run of the workload. Set
//! BCMALLOC_PERF to the command to use, for instance "sudo perf", when perf needs privileges.

use std::{
    collections::BTreeMap,
    fs,
    process::{Command, Stdio},
};

const ALLOCATORS: [&str; 5] = ["system", "bcmalloc", "mimalloc", "jemalloc", "snmalloc"];

/// The best result seen so far for one allocator on one workload.
#[derive(Clone, Copy)]
struct Cell {
    value: f64,
    peak_mib: f64,
}

struct Workload {
    group: String,
    name: String,
    unit: String,
}

impl Workload {
    fn lower_is_better(&self) -> bool {
        self.unit == "MiB"
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| {
        args.iter().position(|arg| arg == flag).and_then(|at| args.get(at + 1)).cloned()
    };
    let quick = args.iter().any(|arg| arg == "--quick");
    let perf = args.iter().any(|arg| arg == "--perf");
    let rounds: usize = value_of("--rounds").and_then(|v| v.parse().ok()).unwrap_or(3);
    let filter = value_of("--filter");
    let baseline = value_of("--baseline").unwrap_or_else(|| "mimalloc".to_string());

    let binaries = build();
    let only = value_of("--only");
    let allocators: Vec<&str> = ALLOCATORS
        .iter()
        .copied()
        .filter(|name| binaries.contains_key(*name))
        .filter(|name| only.as_ref().is_none_or(|list| list.split(',').any(|item| item == *name)))
        .collect();
    let workloads = list(&binaries[allocators[0]], filter.as_deref());

    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    for round in 0..rounds {
        for workload in &workloads {
            for turn in 0..allocators.len() {
                let allocator = allocators[(turn + round) % allocators.len()];
                let Some((cell, counters)) = run(&binaries[allocator], &workload.name, quick, perf)
                else {
                    continue;
                };
                eprintln!(
                    "round {round} {allocator:>9} {:<26} {:>12.2} {}{counters}",
                    workload.name, cell.value, workload.unit
                );
                let key = (allocator.to_string(), workload.name.clone());
                cells
                    .entry(key)
                    .and_modify(|best| {
                        let better = if workload.lower_is_better() {
                            cell.value < best.value
                        } else {
                            cell.value > best.value
                        };
                        best.peak_mib = best.peak_mib.min(cell.peak_mib);
                        if better {
                            best.value = cell.value;
                        }
                    })
                    .or_insert(cell);
            }
        }
    }

    let mut tsv = String::new();
    for ((allocator, name), cell) in &cells {
        let unit = workloads.iter().find(|w| &w.name == name).map_or("", |w| w.unit.as_str());
        tsv += &format!("{allocator}\t{name}\t{:.3}\t{unit}\t{:.1}\n", cell.value, cell.peak_mib);
    }
    let _ = fs::create_dir_all("target");
    let _ = fs::write("target/bench.tsv", tsv);

    print_groups(&allocators, &workloads, &cells);
    print_summary(&allocators, &workloads, &cells, &baseline);
}

/// Builds every benchmark binary and returns the path of each, by allocator.
fn build() -> BTreeMap<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(["bench", "--no-run", "--message-format=json"])
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo runs");
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
        binaries.insert(name, path);
    }
    binaries
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
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (group, name, unit) = (fields.next()?, fields.next()?, fields.next()?);
            let wanted = filter.is_none_or(|texts| {
                texts.split(',').any(|text| name.contains(text) || group.contains(text))
            });
            wanted.then(|| Workload {
                group: group.to_string(),
                name: name.to_string(),
                unit: unit.to_string(),
            })
        })
        .collect()
}

/// The events that --perf counts.
const PERF_EVENTS: &str =
    "task-clock,cycles,instructions,page-faults,L1-dcache-load-misses,dTLB-load-misses";

/// Runs one workload in a fresh process. Returns the result and, with `perf`, a line of counters.
fn run(binary: &str, workload: &str, quick: bool, perf: bool) -> Option<(Cell, String)> {
    let mut command = if perf {
        let launcher = std::env::var("BCMALLOC_PERF").unwrap_or_else(|_| "perf".to_string());
        let mut words = launcher.split_whitespace();
        let mut command = Command::new(words.next()?);
        command.args(words).args(["stat", "-x", ",", "-e", PERF_EVENTS, "--", binary]);
        command
    } else {
        Command::new(binary)
    };
    command.args(["--workload", workload]);
    if quick {
        command.arg("--quick");
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<_> = text.lines().last()?.split('\t').collect();
    let [_, _, value, _, peak] = fields[..] else { return None };
    let cell = Cell { value: value.parse().ok()?, peak_mib: peak.parse().ok()? };
    let mut counters = String::new();
    if perf {
        // perf stat -x writes value,unit,event,... for every event.
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            let fields: Vec<_> = line.split(',').collect();
            if let [value, _, event, ..] = fields[..] {
                counters += &format!("  {event}={value}");
            }
        }
    }
    Some((cell, counters))
}

fn print_groups(
    allocators: &[&str],
    workloads: &[Workload],
    cells: &BTreeMap<(String, String), Cell>,
) {
    let mut groups: Vec<&str> = Vec::new();
    for workload in workloads {
        if !groups.contains(&workload.group.as_str()) {
            groups.push(&workload.group);
        }
    }
    for group in groups {
        let mut rows = vec![
            std::iter::once("Workload".to_string())
                .chain(allocators.iter().map(|a| a.to_string()))
                .collect::<Vec<_>>(),
        ];
        for workload in workloads.iter().filter(|w| w.group == group) {
            let values: Vec<Option<f64>> = allocators
                .iter()
                .map(|a| cells.get(&(a.to_string(), workload.name.clone())).map(|c| c.value))
                .collect();
            let present = values.iter().flatten().copied();
            let best = if workload.lower_is_better() {
                present.fold(f64::MAX, f64::min)
            } else {
                present.fold(0.0, f64::max)
            };
            let mut row = vec![format!("{} ({})", workload.name, workload.unit)];
            for value in values {
                row.push(value.map_or("-".to_string(), |v| {
                    let text = format!("{v:.1}");
                    if (v - best).abs() < 0.05 { format!("**{text}**") } else { text }
                }));
            }
            rows.push(row);
        }
        println!("### {group}\n");
        print_table(&rows);
        println!();
    }
}

/// Prints the geometric mean, per group and overall, of how each allocator compares with the
/// baseline. A figure above 1.00 means better than the baseline for both speed and memory.
fn print_summary(
    allocators: &[&str],
    workloads: &[Workload],
    cells: &BTreeMap<(String, String), Cell>,
    baseline: &str,
) {
    let ratio = |allocator: &str, workload: &Workload, peak: bool| -> Option<f64> {
        let get = |a: &str| {
            cells
                .get(&(a.to_string(), workload.name.clone()))
                .map(|c| if peak { c.peak_mib } else { c.value })
        };
        let (mine, base) = (get(allocator)?, get(baseline)?);
        if mine <= 0.0 || base <= 0.0 {
            return None;
        }
        // Lower is better for memory, so invert to keep "above 1.00 is better".
        Some(if peak || workload.lower_is_better() { base / mine } else { mine / base })
    };
    let geomean = |allocator: &str, pick: &dyn Fn(&Workload) -> Option<bool>| {
        let logs: Vec<f64> =
            workloads.iter().filter_map(|w| ratio(allocator, w, pick(w)?)?.ln().into()).collect();
        (!logs.is_empty()).then(|| (logs.iter().sum::<f64>() / logs.len() as f64).exp())
    };

    let mut groups: Vec<&str> = Vec::new();
    for workload in workloads {
        if !groups.contains(&workload.group.as_str()) {
            groups.push(&workload.group);
        }
    }
    let mut rows = vec![
        std::iter::once(format!("Compared with {baseline}"))
            .chain(allocators.iter().map(|a| a.to_string()))
            .collect::<Vec<_>>(),
    ];
    let mut add = |label: String, pick: &dyn Fn(&Workload) -> Option<bool>| {
        let mut row = vec![label];
        row.extend(
            allocators
                .iter()
                .map(|a| geomean(a, pick).map_or("-".to_string(), |g| format!("{g:.2}"))),
        );
        rows.push(row);
    };
    let mut per_group: Vec<Vec<f64>> = vec![Vec::new(); allocators.len()];
    for group in groups.iter().filter(|g| **g != "memory") {
        add(format!("speed, {group}"), &|w| {
            (w.group == *group && !w.lower_is_better()).then_some(false)
        });
        for (at, allocator) in allocators.iter().enumerate() {
            let pick = |w: &Workload| (w.group == *group && !w.lower_is_better()).then_some(false);
            per_group[at].extend(geomean(allocator, &pick));
        }
    }
    add("speed, all workloads".to_string(), &|w| (!w.lower_is_better()).then_some(false));
    add("memory held afterwards".to_string(), &|w| w.lower_is_better().then_some(false));
    add("peak resident memory".to_string(), &|w| (!w.lower_is_better()).then_some(true));
    // Every group counts once, so a group with many workloads does not dominate.
    let mut row = vec!["speed, mean of groups".to_string()];
    row.extend(per_group.iter().map(|means| {
        if means.is_empty() {
            return "-".to_string();
        }
        let mean = (means.iter().map(|m| m.ln()).sum::<f64>() / means.len() as f64).exp();
        format!("{mean:.2}")
    }));
    rows.push(row);
    println!("### Summary\n");
    print_table(&rows);
}

fn print_table(rows: &[Vec<String>]) {
    let widths: Vec<usize> =
        (0..rows[0].len()).map(|i| rows.iter().map(|r| r[i].len()).max().unwrap()).collect();
    let line = |row: &Vec<String>| {
        let cells: Vec<_> =
            row.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
        println!("| {} |", cells.join(" | "));
    };
    line(&rows[0]);
    println!("| {} |", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join(" | "));
    rows[1..].iter().for_each(line);
}
