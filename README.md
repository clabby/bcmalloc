# bcmalloc

<img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"> <img src="https://img.shields.io/badge/rust-1.100%2B-orange.svg" alt="Rust 1.100+"> <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg" alt="Platforms">

A global allocator for long-lived, highly concurrent Rust programs. It reserves one region of address space up front and serves small blocks from per-thread stacks of free pointers, without atomic operations and without touching the blocks. Blocks freed by another thread go home in batched messages, one compare-and-swap per batch. Memory freed by one size class is reused by every other, memory that stays idle goes back to the operating system, and the capacity you configure is a hard limit.

Declare it as the global allocator, and the region is reserved the first time anything allocates.

```rust
use bcmalloc::Bcmalloc;

#[global_allocator]
static GLOBAL: Bcmalloc = Bcmalloc::new();

let numbers: Vec<u64> = (0..1000).collect();
assert_eq!(numbers.iter().sum::<u64>(), 499_500);
```

To size the region for your program, pass a [`Config`]. Every method is `const`, so the configuration sits in the static.

```rust
use bcmalloc::{Commit, Config, Bcmalloc};

#[global_allocator]
static GLOBAL: Bcmalloc = Bcmalloc::with_config(
    Config::new().capacity(8 << 30).max_heaps(256).commit(Commit::Lazy),
);

let text = String::from("allocated from an 8 GiB region");
assert_eq!(GLOBAL.stats().unwrap().heaps_in_use, 1);
# drop(text);
```

The same allocator is available without the global hook. A [`Mapping`] owns a region, and each thread takes a [`Heap`] from it.

```rust
use core::alloc::Layout;
use bcmalloc::{Config, Mapping};

let mapping = Mapping::new(&Config::new().capacity(64 << 20).max_heaps(4))?;
let heap = mapping.region().heap().expect("a free heap");

let layout = Layout::new::<[u64; 8]>();
let block = heap.alloc(layout).expect("memory");
// SAFETY: the block came from this region with this layout and is not used again.
unsafe { heap.dealloc(block, layout) };
# Ok::<(), bcmalloc::Error>(())
```

The allocator also implements the standard library's `Allocator` trait, which has been stable since Rust 1.100. A collection can then take it as a parameter without it being the global allocator. A heap implements the trait too, for code that wants one thread's heap.

```rust
use bcmalloc::Bcmalloc;

static ALLOCATOR: Bcmalloc = Bcmalloc::new();

let mut numbers = Vec::new_in(&ALLOCATOR);
numbers.extend(0..1000_u32);
assert_eq!(numbers.iter().sum::<u32>(), 499_500);
```

## Metrics

With the `metrics` feature, every heap counts its allocations and frees by size class, the slabs it holds, and the slow paths it takes, and `Bcmalloc::metrics` sums the counts into a snapshot that prints as a report. Only a heap's own thread writes its counters, with plain adds on cache lines the allocator touches anyway, so the feature costs little enough to leave on in production.

```rust,ignore
use bcmalloc::Bcmalloc;

#[global_allocator]
static GLOBAL: Bcmalloc = Bcmalloc::new();

if let Some(metrics) = GLOBAL.metrics() {
    eprintln!("{metrics}");
}
```

The C library prints the same report at exit when it is built with the feature and `BCMALLOC_SHOW_METRICS` is set.

## Configuration

| Option       | Default | What it does                                                         |
| ------------ | ------- | -------------------------------------------------------------------- |
| `capacity`   | 64 GiB  | Size of the region. It is address space until memory is touched.     |
| `max_heaps`  | 4096    | How many threads can allocate at once.                               |
| `commit`     | `Lazy`  | `Populate` backs every page now and `Lock` also pins them in RAM.    |
| `huge_pages` | on      | Huge pages on Linux for big heaps and big blocks. macOS has none.    |
| `numa`       | on      | Splits the region between NUMA nodes on Linux machines with several. |

If the operating system refuses the default reservation, the global allocator retries with half the capacity down to 256 MiB.

## Benchmarks

![For every suite and every other allocator, one dot per workload showing how much faster bcmalloc is](assets/benchmarks.svg)

<!-- generated:overview -->
| Compared with mimalloc             | system | bcmalloc | jemalloc | snmalloc |
| ---------------------------------- | ------ | -------- | -------- | -------- |
| development suite, macOS, speed    | 0.52   | 2.09     | 0.86     | 1.24     |
| development suite, Linux, speed    | 0.64   | 1.82     | 0.89     | 1.18     |
| Rust programs, macOS, speed        | 0.94   | 1.03     | 0.96     | 1.04     |
| Rust programs, Linux, speed        | 0.72   | 1.13     | 1.03     | 1.05     |
| mimalloc-bench, Linux, speed       | -      | 1.02     | 0.82     | 0.94     |
| mimalloc-bench, Linux, peak memory | -      | 1.49     | 1.37     | 1.40     |
<!-- /generated:overview -->

The development suite has 51 synthetic workloads and is what the allocator is tuned against. The Rust programs, built on serde_json, regex, syn, hyper, tokio, fjall and rayon, and the C programs of mimalloc-bench are never tuned against. Figures are relative to mimalloc, and above 1.00 is better, faster or less memory. [BENCHMARKS.md](https://github.com/clabby/bcmalloc/blob/main/BENCHMARKS.md) has the method, the machines, every table, and the programs where bcmalloc is not the fastest.

## Limits

- Alignments above 64 KiB are refused, and the allocation fails.
- A thread that finds all `max_heaps` heaps taken fails its allocations, which aborts the program.
- The capacity is a hard limit. Nothing grows the region after start-up.
- Linux and macOS are supported. Windows is not.
- The standard library's `Allocator` trait needs Rust 1.100 or later, which is the minimum supported version.

## Testing

The lock-free code is checked with loom, the unsafe code with Miri under strict provenance, and the allocator and its chunk bitmap with fuzz targets that compare them against reference models. Line coverage of the unit tests is reported to Codecov. `just` has a recipe for each, and `just --list` shows them all.

## LLM Disclosure

bcmalloc's implementation and design was aided by LLMs (namely Claude Opus 5.5 and OpenAI's Sol 6.1).

## License

Licensed under the Apache License, Version 2.0.

[`Config`]: crate::Config
[`Mapping`]: crate::Mapping
[`Heap`]: crate::Heap
