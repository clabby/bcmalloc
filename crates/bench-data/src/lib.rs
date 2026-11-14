//! The benchmark results of bcmalloc, kept in `benchmarks.json` at the root of the repository,
//! and everything that is rendered from them.
//!
//! The benchmark commands record their results here: `just bench` and `just bench-apps` through
//! the report driver, and `just mimalloc-bench` through `bench-data import-mimalloc-bench`. Each
//! records one run per suite and machine, replacing the previous one. The Markdown tables in
//! `BENCHMARKS.md` and `README.md` and the figure in `assets/benchmarks.svg` are rendered from
//! the file, and `bench-data check` fails if they are out of date.

pub mod docs;
pub mod machine;
pub mod markdown;
pub mod model;
pub mod svg;

pub use model::{Data, Metric, Run, Stat, Workload};
