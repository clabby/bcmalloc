# cwmalloc

<img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"> <img src="https://img.shields.io/badge/rust-1.85%2B-orange.svg" alt="Rust 1.85+"> <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg" alt="Platforms">

A global allocator for long-lived, highly concurrent Rust programs. It reserves one region of address space up front and serves small blocks from thread-local heaps without atomic operations. A free from another thread costs one compare-and-swap. Memory freed by one size class is reused by every other, and the capacity you configure is a hard limit.

Usage is one line. The region is reserved the first time anything allocates.

```rust
use cwmalloc::Cwmalloc;

#[global_allocator]
static GLOBAL: Cwmalloc = Cwmalloc::new();

let numbers: Vec<u64> = (0..1000).collect();
assert_eq!(numbers.iter().sum::<u64>(), 499_500);
```

To size the region for your program, pass a [`Config`]. Every method is `const`, so the configuration sits in the static.

```rust
use cwmalloc::{Commit, Config, Cwmalloc};

#[global_allocator]
static GLOBAL: Cwmalloc = Cwmalloc::with_config(
    Config::new().capacity(8 << 30).max_heaps(256).commit(Commit::Lazy),
);

let text = String::from("allocated from an 8 GiB region");
assert_eq!(GLOBAL.stats().unwrap().heaps_in_use, 1);
# drop(text);
```

The same allocator is available without the global hook. A [`Mapping`] owns a region, and each thread takes a [`Heap`] from it.

```rust
use core::alloc::Layout;
use cwmalloc::{Config, Mapping};

let mapping = Mapping::new(&Config::new().capacity(64 << 20).max_heaps(4))?;
let heap = mapping.region().heap().expect("a free heap");

let layout = Layout::new::<[u64; 8]>();
let block = heap.alloc(layout).expect("memory");
// SAFETY: the block came from this region with this layout and is not used again.
unsafe { heap.dealloc(block, layout) };
# Ok::<(), cwmalloc::Error>(())
```

## Configuration

| Option       | Default | What it does                                                      |
| ------------ | ------- | ----------------------------------------------------------------- |
| `capacity`   | 64 GiB  | Size of the region. It is address space until memory is touched.  |
| `max_heaps`  | 4096    | How many threads can allocate at once.                            |
| `commit`     | `Lazy`  | `Populate` backs every page now and `Lock` also pins them in RAM. |
| `huge_pages` | off     | Asks Linux for transparent huge pages. macOS ignores it.          |

If the operating system refuses the default reservation, the global allocator retries with half the capacity down to 256 MiB.

## Benchmarks

Millions of operations per second, and MiB for the last row. Each figure is the best of five runs on an Apple M4 Max with macOS 26.5 and Rust 1.100. The system allocator is macOS libmalloc. Run `just bench` to reproduce them.

| Workload                      | system     | cwmalloc | mimalloc  | jemalloc   | snmalloc  |
| ----------------------------- | ---------- | -------- | --------- | ---------- | --------- |
| single-thread churn           | 36.6       | 63.5     | **122.4** | 112.3      | 119.1     |
| 4-thread churn                | 39.7       | 230.9    | **462.3** | 435.9      | 341.2     |
| 8-thread churn                | 64.8       | 393.2    | 594.7     | **841.0**  | 579.8     |
| 16-thread churn               | 108.7      | 637.4    | 927.2     | **1192.2** | 810.9     |
| 1-pair cross-thread free      | 29.4       | 35.9     | 25.4      | **61.9**   | 44.9      |
| 4-pair cross-thread free      | 63.3       | 75.7     | 68.9      | **168.7**  | 129.7     |
| vec growth                    | **1354.4** | 665.0    | 657.5     | 1252.3     | 858.9     |
| binary trees                  | 66.0       | 109.2    | 123.6     | 59.3       | **139.1** |
| large blocks                  | 0.2        | **0.2**  | 0.2       | 0.0        | 0.0       |
| peak memory (lower is better) | 863.5      | 61.6     | 90.2      | **47.1**   | 66.5      |

cwmalloc is ahead of the system allocator on every row except vec growth and uses less memory than mimalloc. It is within a factor of 2.3 of the fastest allocator on every throughput row. It is slowest relative to the others on blocks between 512 B and 2 KiB spread over many slabs. DESIGN.md explains where the time goes.

## Limits

- Alignments above 64 KiB are refused, and the allocation fails.
- A thread that finds all `max_heaps` heaps taken fails its allocations, which aborts the program.
- The capacity is a hard limit. Nothing grows the region after start-up.
- Linux and macOS are supported. Windows is not.

## Testing

The lock-free code is checked with loom, the unsafe code with Miri under strict provenance, and the allocator and its chunk bitmap with fuzz targets. `just` has a recipe for each, and `just --list` shows them all.

## License

Licensed under the Apache License, Version 2.0.

[`Config`]: crate::Config
[`Mapping`]: crate::Mapping
[`Heap`]: crate::Heap
