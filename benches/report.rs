//! Prints the results of the allocator benchmarks as a Markdown table.
//!
//! Usage is `cargo bench --bench report -- target/bench.tsv`, where the file holds the rows that
//! the other benchmarks print to stdout.

use std::{collections::BTreeMap, fs};

const ALLOCATORS: [&str; 5] = ["system", "cwmalloc", "mimalloc", "jemalloc", "snmalloc"];

fn main() {
    let path =
        std::env::args().find(|arg| arg.ends_with(".tsv")).expect("pass a .tsv results file");
    let results = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));

    // workload -> allocator -> value, in the order the workloads first appear.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut table: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for row in results.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        let [allocator, workload, value, unit] = fields[..] else { continue };
        if !order.iter().any(|(name, _)| name == workload) {
            order.push((workload.to_string(), unit.to_string()));
        }
        table
            .entry(workload.to_string())
            .or_default()
            .insert(allocator.to_string(), value.parse().unwrap());
    }

    let columns: Vec<_> =
        ALLOCATORS.iter().filter(|a| results.contains(&format!("{a}\t"))).collect();
    let mut rows = vec![
        std::iter::once("Workload".to_string())
            .chain(columns.iter().map(|a| a.to_string()))
            .collect::<Vec<_>>(),
    ];
    for (workload, unit) in &order {
        let values = &table[workload];
        let lower_is_better = unit == "MiB";
        let best =
            values.values().copied().fold(if lower_is_better { f64::MAX } else { 0.0 }, |a, b| {
                if lower_is_better { a.min(b) } else { a.max(b) }
            });
        let mut row = vec![format!("{workload} ({unit})")];
        for allocator in &columns {
            row.push(values.get(**allocator).map_or("-".to_string(), |value| {
                let text = format!("{value:.1}");
                if (value - best).abs() < f64::EPSILON { format!("**{text}**") } else { text }
            }));
        }
        rows.push(row);
    }

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
