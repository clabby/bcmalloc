//! The Markdown tables: the report of a run, the overview of every run, and the table of
//! mimalloc-bench.

use crate::model::{Data, LATENCY, Run, Workload};

/// Formats a value with three significant digits or one decimal, as the tables show it.
pub fn fmt_value(value: f64) -> String {
    if value.abs() >= 10.0 { format!("{value:.1}") } else { format!("{value:.2}") }
}

/// Lays out rows as a Markdown table with aligned columns. The first row is the header.
pub fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|row| row[i].chars().count()).max().unwrap_or(0))
        .collect();
    let line = |row: &Vec<String>| {
        let cells: Vec<String> =
            row.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
        format!("| {} |\n", cells.join(" | "))
    };
    let mut out = line(&rows[0]);
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    out += &format!("| {} |\n", rule.join(" | "));
    rows[1..].iter().for_each(|row| out += &line(row));
    out
}

/// Which cells hold the best median or one whose interval overlaps the best one's interval.
pub fn leaders(cells: &[Option<(f64, f64, f64)>], lower: bool) -> Vec<bool> {
    let better = |a: f64, b: f64| if lower { a < b } else { a > b };
    let mut best: Option<(f64, f64, f64)> = None;
    for cell in cells.iter().flatten() {
        if best.is_none_or(|(median, ..)| better(cell.0, median)) {
            best = Some(*cell);
        }
    }
    cells
        .iter()
        .map(|cell| match (cell, best) {
            (Some((_, lo, hi)), Some((_, best_lo, best_hi))) => *lo <= best_hi && best_lo <= *hi,
            _ => false,
        })
        .collect()
}

/// The groups of a run, in the order they first appear.
fn groups(run: &Run) -> Vec<&str> {
    let mut groups: Vec<&str> = Vec::new();
    for workload in &run.workloads {
        if !groups.contains(&workload.group.as_str()) {
            groups.push(&workload.group);
        }
    }
    groups
}

fn header(first: &str, allocators: &[String]) -> Vec<String> {
    std::iter::once(first.to_string()).chain(allocators.iter().cloned()).collect()
}

/// A table of one metric of some workloads, with the leaders in bold.
fn metric_table(run: &Run, members: &[&Workload], metric: &dyn Fn(&Workload) -> String) -> String {
    let mut rows = vec![header("Workload", &run.allocators)];
    for workload in members {
        let metric = metric(workload);
        let cells: Vec<_> = run
            .allocators
            .iter()
            .map(|a| workload.stat(&metric, a).map(|s| (s.median, s.lo, s.hi)))
            .collect();
        let bold = leaders(&cells, workload.lower_is_better(&metric));
        let mut row = vec![format!("{} ({})", workload.name, workload.unit(&metric))];
        for (cell, bold) in cells.iter().zip(bold) {
            row.push(cell.map_or("-".to_string(), |(median, ..)| {
                let text = fmt_value(median);
                if bold { format!("**{text}**") } else { text }
            }));
        }
        rows.push(row);
    }
    table(&rows)
}

/// The tables of every group of a run: the primary metric, the cold pass and the latencies.
pub fn groups_report(run: &Run) -> String {
    let mut out = String::new();
    for group in groups(run) {
        let members: Vec<&Workload> = run.workloads.iter().filter(|w| w.group == group).collect();
        out += &format!("### {group}\n\n{}\n", metric_table(run, &members, &|w| w.primary.clone()));
        let has = |w: &Workload, metric: &str| w.metrics.contains_key(metric);
        let cold: Vec<&Workload> = members.iter().copied().filter(|w| has(w, "cold")).collect();
        if !cold.is_empty() {
            let cold_table = metric_table(run, &cold, &|_| "cold".to_string());
            out += &format!("### {group}, cold\n\n{cold_table}\n");
        }
        let latency: Vec<&Workload> = members.iter().copied().filter(|w| has(w, "p99")).collect();
        if !latency.is_empty() {
            let mut rows = vec![header("Workload", &run.allocators)];
            for workload in latency {
                let mut row = vec![format!("{} ({})", workload.name, workload.unit("p99"))];
                for allocator in &run.allocators {
                    let cells: Vec<String> = LATENCY
                        .iter()
                        .map(|metric| {
                            workload
                                .stat(metric, allocator)
                                .map_or("-".to_string(), |stat| fmt_value(stat.median))
                        })
                        .collect();
                    row.push(cells.join(" / "));
                }
                rows.push(row);
            }
            out += &format!("### {group}, latency (p50 / p99 / p999)\n\n{}\n", table(&rows));
        }
    }
    out
}

/// How `allocator` compares with the baseline on a metric of a workload, as a ratio of medians
/// where above 1 is better. With `peak` it compares the peak resident memory instead.
pub fn ratio(
    run: &Run,
    workload: &Workload,
    metric: &str,
    allocator: &str,
    peak: bool,
) -> Option<f64> {
    let get = |a: &str| {
        let stat = workload.stat(metric, a)?;
        Some(if peak { stat.peak_mib } else { stat.median })
    };
    let (mine, base) = (get(allocator)?, get(&run.baseline)?);
    if mine <= 0.0 || base <= 0.0 {
        return None;
    }
    Some(if peak || workload.lower_is_better(metric) { base / mine } else { mine / base })
}

/// The geometric mean of some values, or `None` if there are none.
pub fn geomean(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let logs: Vec<f64> = values.into_iter().map(f64::ln).collect();
    (!logs.is_empty()).then(|| (logs.iter().sum::<f64>() / logs.len() as f64).exp())
}

/// The geometric mean of how `allocator` compares with the baseline on the speed workloads.
pub fn speed(run: &Run, allocator: &str) -> Option<f64> {
    geomean(
        run.workloads
            .iter()
            .filter(|w| w.is_speed())
            .filter_map(|w| ratio(run, w, &w.primary, allocator, false)),
    )
}

/// The geometric mean of how the peak resident memory of `allocator` compares with the
/// baseline's on the speed workloads, where above 1 means less memory.
pub fn memory(run: &Run, allocator: &str) -> Option<f64> {
    geomean(
        run.workloads
            .iter()
            .filter(|w| w.is_speed())
            .filter_map(|w| ratio(run, w, &w.primary, allocator, true)),
    )
}

/// The summary of a run: per group and overall, the geometric mean of how each allocator compares
/// with the baseline. A figure above 1.00 means better for speed, latency and memory alike.
pub fn summary(run: &Run) -> String {
    type Pick<'a> = &'a dyn Fn(&Workload) -> Option<(String, bool)>;
    let mean = |allocator: &str, pick: Pick<'_>| {
        geomean(run.workloads.iter().filter_map(|w| {
            let (metric, peak) = pick(w)?;
            ratio(run, w, &metric, allocator, peak)
        }))
    };
    let row = |label: String, pick: Pick<'_>| {
        let mut row = vec![label];
        row.extend(
            run.allocators
                .iter()
                .map(|a| mean(a, pick).map_or("-".to_string(), |g| format!("{g:.2}"))),
        );
        row
    };
    let speed = |w: &Workload| w.is_speed().then(|| (w.primary.clone(), false));
    let held = |w: &Workload| (w.size && w.primary != "p99").then(|| (w.primary.clone(), false));

    let mut rows = vec![header(&format!("Compared with {}", run.baseline), &run.allocators)];
    let mut per_group: Vec<Vec<f64>> = vec![Vec::new(); run.allocators.len()];
    let groups = groups(run);
    let multiple = groups.len() > 1;
    for group in &groups {
        if !multiple || !run.workloads.iter().any(|w| w.group == *group && w.is_speed()) {
            continue;
        }
        let pick = |w: &Workload| if w.group == *group { speed(w) } else { None };
        rows.push(row(format!("speed, {group}"), &pick));
        for (at, allocator) in run.allocators.iter().enumerate() {
            per_group[at].extend(mean(allocator, &pick));
        }
    }
    rows.push(row("speed, all workloads".to_string(), &speed));
    if run.workloads.iter().any(|w| w.size) {
        rows.push(row("memory held afterwards".to_string(), &held));
    }
    rows.push(row("peak resident memory".to_string(), &|w| {
        speed(w).map(|(metric, _)| (metric, true))
    }));
    if multiple {
        let mut mean_row = vec!["speed, mean of groups".to_string()];
        mean_row.extend(per_group.iter().map(|means| {
            geomean(means.iter().copied()).map_or("-".to_string(), |g| format!("{g:.2}"))
        }));
        rows.push(mean_row);
    }
    let has = |w: &Workload, metric: &str| w.metrics.contains_key(metric);
    if run.workloads.iter().any(|w| has(w, "cold")) {
        rows.push(row("speed, cold".to_string(), &|w| {
            (has(w, "cold") && !w.lower).then(|| ("cold".to_string(), false))
        }));
    }
    if run.workloads.iter().any(|w| has(w, "p99")) {
        rows.push(row("latency, p99".to_string(), &|w| {
            has(w, "p99").then(|| ("p99".to_string(), false))
        }));
    }
    table(&rows)
}

/// The full report of a run, as the report driver prints it.
pub fn report(run: &Run) -> String {
    format!(
        "Median of {} runs. Bold marks the best median and every median whose 95% bootstrap \
         interval overlaps it.\n\n{}### Summary\n\n{}",
        run.samples,
        groups_report(run),
        summary(run)
    )
}

/// The table of a mimalloc-bench run: wall time and peak memory of every program, with the best
/// of each in bold.
pub fn mimalloc_bench(run: &Run) -> String {
    let mut rows = vec![header("Program", &run.allocators)];
    for workload in &run.workloads {
        let stats: Vec<_> = run.allocators.iter().map(|a| workload.stat("time", a)).collect();
        let best_time = stats.iter().flatten().map(|s| s.median).fold(f64::INFINITY, f64::min);
        let best_peak = stats.iter().flatten().map(|s| s.peak_mib).fold(f64::INFINITY, f64::min);
        let mut row = vec![workload.name.clone()];
        for stat in stats {
            row.push(stat.map_or("-".to_string(), |stat| {
                let bold =
                    |text: String, best: bool| if best { format!("**{text}**") } else { text };
                format!(
                    "{} / {}",
                    bold(format!("{:.3}", stat.median), stat.median <= best_time),
                    bold(format!("{:.1}", stat.peak_mib), stat.peak_mib <= best_peak)
                )
            }));
        }
        rows.push(row);
    }
    let mut means = vec![header(&format!("Compared with {}", run.baseline), &run.allocators)];
    let cell = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}"));
    let speed_row = run.allocators.iter().map(|a| cell(speed(run, a)));
    means.push(std::iter::once("speed".to_string()).chain(speed_row).collect());
    let memory_row = run.allocators.iter().map(|a| cell(memory(run, a)));
    means.push(std::iter::once("peak resident memory".to_string()).chain(memory_row).collect());
    format!("{}\n{}", table(&means), table(&rows))
}

/// The display name of a suite.
pub fn suite_name(suite: &str) -> &str {
    match suite {
        "dev" => "development suite",
        "apps" => "Rust programs",
        other => other,
    }
}

/// The display name of a machine key.
pub fn machine_name(machine: &str) -> &str {
    match machine {
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
}

/// The overview of every run: the speed of each allocator against the baseline, and the peak
/// memory where the suite is about memory as much as speed.
pub fn overview(data: &Data) -> String {
    let mut columns: Vec<String> = Vec::new();
    for run in &data.runs {
        for allocator in &run.allocators {
            if *allocator != run.baseline && !columns.contains(allocator) {
                columns.push(allocator.clone());
            }
        }
    }
    let baseline = data.runs.first().map_or("mimalloc", |run| run.baseline.as_str());
    let mut rows = vec![header(&format!("Compared with {baseline}"), &columns)];
    let cell = |value: Option<f64>| value.map_or("-".to_string(), |v| format!("{v:.2}"));
    for run in &data.runs {
        let label = format!("{}, {}", suite_name(&run.suite), machine_name(&run.machine));
        let speeds = columns.iter().map(|a| cell(speed(run, a)));
        rows.push(std::iter::once(format!("{label}, speed")).chain(speeds).collect());
        if run.suite == "mimalloc-bench" {
            let memory = columns.iter().map(|a| cell(memory(run, a)));
            rows.push(std::iter::once(format!("{label}, peak memory")).chain(memory).collect());
        }
    }
    table(&rows)
}

/// The machines, with the day of their runs.
pub fn machines(data: &Data) -> String {
    let mut rows = vec![vec![
        "Suite".to_string(),
        "Machine".to_string(),
        "Runs".to_string(),
        "Date".to_string(),
    ]];
    for run in &data.runs {
        rows.push(vec![
            format!("{}, {}", suite_name(&run.suite), machine_name(&run.machine)),
            run.description.clone(),
            run.samples.to_string(),
            run.date.clone(),
        ]);
    }
    table(&rows)
}
