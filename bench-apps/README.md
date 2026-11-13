# Benchmark apps

This crate holds real Rust programs that run under the same allocators as the main benchmarks. They are a held-out set. The allocator is never tuned against them. They exist to catch tuning that helps the synthetic loops of the main crate and does nothing for ordinary programs.

The crate has its own empty `[workspace]` table, so it stays out of the build of the main crate. It depends on bcmalloc by path and on the same competitors, mimalloc, snmalloc and jemalloc, plus the application crates.

## Workloads

All of them are in the group `apps`, build their inputs in memory and use no files and no network beyond loopback. A pass takes between a third and a second on an Apple M4.

| Workload       | Program                                                                  | Unit   |
| -------------- | ------------------------------------------------------------------------ | ------ |
| json           | serde_json parses a large document, rebuilds it, and writes it out again | MB/s   |
| regex          | The regex crate compiles about thirty patterns and scans a few MB of text | MB/s   |
| syn            | syn parses generated Rust source, a visitor walks it, and quote prints it | MB/s   |
| http           | A hyper server and eight keep-alive clients exchange small JSON bodies   | kreq/s |
| tokio channels | Tasks pass boxed messages through tokio channels on four worker threads  | Mmsg/s |

Every workload prints a cold line, the first pass in a fresh process, and a warm line, a second measured pass.

## Running

Build every binary, then run one workload by hand with the binary of the allocator you want. There is one binary per allocator: system, bcmalloc, mimalloc, jemalloc and snmalloc.

```sh
cargo bench --no-run --manifest-path bench-apps/Cargo.toml
cargo bench --manifest-path bench-apps/Cargo.toml --bench bcmalloc -- --workload json
cargo bench --manifest-path bench-apps/Cargo.toml --bench mimalloc -- --workload "tokio channels" --quick
```

The binaries speak version 2 of the benchmark protocol.

| Argument        | Effect                                                           |
| --------------- | ---------------------------------------------------------------- |
| --list          | Print one line per workload: group, name, unit and better.       |
| --workload NAME | Run only that workload.                                          |
| --quick         | Do a quarter of the work.                                        |

A result line has the allocator, the workload, the metric, the value, the unit and the peak memory of the process in MiB, separated by tabs. Higher is better for every workload here.

```text
bcmalloc	json	cold	287.756	MB/s	54.8
bcmalloc	json	warm	284.666	MB/s	54.8
```

Each run is a new process, so build the binaries once and start them as often as you like. Cargo prints their paths under `bench-apps/target/release/build/` when it builds them.
