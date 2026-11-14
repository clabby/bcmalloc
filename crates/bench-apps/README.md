# Benchmark apps

This crate holds real Rust programs that run under the same allocators as the synthetic benchmarks in crates/bench. They are a held-out set. The allocator is never tuned against them. They exist to catch tuning that helps the synthetic loops and does nothing for ordinary programs.

It is a member of the workspace that is never published. It depends on bcmalloc and on the same competitors, mimalloc, snmalloc and jemalloc, plus the application crates. `just bench-apps` runs it with the benchmark driver, which takes the same options as `just bench`.

## Workloads

All of them are in the group `apps`, generate deterministic inputs in memory and use no network beyond loopback. The KV workload uses a temporary directory, removed after each pass. Full passes target roughly 0.3–1.5 seconds on an Apple M4 Max; setup and teardown can add wall time.

| Workload       | Program                                                                  | Unit   |
| -------------- | ------------------------------------------------------------------------ | ------ |
| json           | serde_json parses a large document, rebuilds it, and writes it out again | MB/s   |
| regex          | The regex crate compiles about thirty patterns and scans a few MB of text | MB/s   |
| syn            | syn parses generated Rust source, a visitor walks it, and quote prints it | MB/s   |
| http           | A hyper server and eight keep-alive clients exchange small JSON bodies   | kreq/s |
| kv store       | Fjall with 16 or fewer threads mixes inserts, reads, 16-entry scans and deletes over 256 MiB | ops/s |
| http many      | 256 keep-alive hyper clients exchange varied JSON bodies on up to 16 workers | req/s |
| rayon          | Up to 32 workers build nested records, sort String keys and group records before caller-thread frees | records/s |
| tokio channels | Tasks pass boxed messages through tokio channels on four worker threads  | Mmsg/s |

Every workload prints a cold line, the first pass in a fresh process, and a warm line, a second measured pass.

## Running

Build every binary, then run one workload by hand with the binary of the allocator you want. There is one binary per allocator: system, bcmalloc, mimalloc, jemalloc and snmalloc.

```sh
cargo bench -p bcmalloc-bench-apps --no-run
cargo bench -p bcmalloc-bench-apps --bench bcmalloc -- --workload json
cargo bench -p bcmalloc-bench-apps --bench mimalloc -- --workload "tokio channels" --quick
```

The binaries speak version 2 of the benchmark protocol.

| Argument        | Effect                                                           |
| --------------- | ---------------------------------------------------------------- |
| --list          | Print one line per workload: group, name, unit and better.       |
| --workload NAME | Run only that workload.                                          |
| --quick         | Do a quarter of the work.                                        |

A result line has the allocator, the workload, the metric, the value, the unit and the peak memory of the process in MiB, separated by tabs. Higher is better for throughput. `http many` also prints `p99` in `us` (lower is better), the nearest-rank 99th percentile of individual warm-pass requests, including JSON encoding and decoding. Connection setup is outside the clock.

`kv store` preloads 65,536 records of 4 KiB outside the clock, then performs 160,000 mixed operations (40% inserts, 40% point reads, 10% scans, 10% deletes). A scan counts as one operation. Writes use Fjall’s buffered journal; fsync and directory cleanup are outside the clock. Quick mode quarters measured operations while preserving the live set. `http many` sends 256 requests per connection (64 quick); `rayon` processes 1,200,000 records (300,000 quick). Runtime and pool creation are outside the clock; the Rayon clock includes dropping the grouped records on the caller thread.

```text
bcmalloc	json	cold	287.756	MB/s	54.8
bcmalloc	json	warm	284.666	MB/s	54.8
```

Each run is a new process, so build the binaries once and start them as often as you like. Cargo prints their paths under `target/release/build/` when it builds them.
