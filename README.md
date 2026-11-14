# bcmalloc

<img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"> <img src="https://img.shields.io/badge/rust-1.88%2B-orange.svg" alt="Rust 1.88+"> <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg" alt="Platforms"> <a href="https://codecov.io/gh/clabby/bcmalloc"><img src="https://codecov.io/gh/clabby/bcmalloc/graph/badge.svg" alt="Coverage"></a>

bcmalloc is a global allocator for Rust programs that run for a long time on many threads. It reserves its address space once, at a size you choose, and never grows past it, so the program's memory has a hard ceiling, while the pages inside are only backed as they are used and go back to the operating system when they fall idle. Each thread allocates and frees through its own heap without atomic instructions, and blocks freed on another thread go home in batches, one compare-and-swap per batch. It is faster than mimalloc, jemalloc and snmalloc on [mimalloc-bench](https://github.com/daanx/mimalloc-bench) and on the held-out Rust programs on Linux, and on mimalloc-bench it holds about two thirds of mimalloc's peak memory. A program that wants its memory resident for its whole run can back or pin the whole region before its first allocation with [`Commit::Populate`](https://docs.rs/bcmalloc/latest/bcmalloc/enum.Commit.html#variant.Populate) or [`Commit::Lock`](https://docs.rs/bcmalloc/latest/bcmalloc/enum.Commit.html#variant.Lock). [DESIGN.md](https://github.com/clabby/bcmalloc/blob/main/DESIGN.md) explains how it works, and [BENCHMARKS.md](https://github.com/clabby/bcmalloc/blob/main/BENCHMARKS.md) has every measurement.

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

With the `allocator-api` feature, the allocator also implements the standard library's `Allocator` trait, which has been stable since Rust 1.100. A collection can then take it as a parameter without it being the global allocator. A heap implements the trait too, for code that wants one thread's heap.

```rust
# #[cfg(feature = "allocator-api")] {
use bcmalloc::Bcmalloc;

static ALLOCATOR: Bcmalloc = Bcmalloc::new();

let mut numbers = Vec::new_in(&ALLOCATOR);
numbers.extend(0..1000_u32);
assert_eq!(numbers.iter().sum::<u32>(), 499_500);
# }
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

| Option       | Default | What it does                                                                     |
| ------------ | ------- | -------------------------------------------------------------------------------- |
| `capacity`   | 64 GiB  | Size of the region. It is address space until memory is touched.                 |
| `max_heaps`  | 4096    | How many threads can allocate at once.                                           |
| `commit`     | `Lazy`  | `Populate` backs every page now and `Lock` also pins them in RAM.                |
| `huge_pages` | on      | Huge pages on Linux for big heaps, big processes and big blocks. macOS has none. |
| `numa`       | on      | Splits the region between NUMA nodes on Linux machines with several.             |

## Benchmarks

![For every suite and every other allocator, one dot per workload showing how much faster bcmalloc is](https://raw.githubusercontent.com/clabby/bcmalloc/main/assets/benchmarks.svg)

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

- The capacity is a hard limit. Nothing grows the region after start-up, and an allocation that does not fit fails. Through the global allocator, a failed allocation aborts the program.
- The default region is 64 GiB of address space. Under strict overcommit (`vm.overcommit_memory=2`), or with a limit on a process's data or address space, the kernel counts all of it. The global allocator then retries with half the capacity until the reservation fits, down to 256 MiB.
- Alignments above 4 MiB are refused, and the allocation fails. A request aligned to more than 64 KiB takes a run of whole chunks of its own, at least 128 KiB, whatever its size.
- A thread that finds all `max_heaps` heaps taken fails its allocations.
- Idle memory goes back to the operating system from the allocator's slower operations, such as refilling a thread's cache, and not from a background thread. A program that goes quiet keeps what it freed until it allocates enough to reach one of them, or until it calls [`Bcmalloc::trim`](https://docs.rs/bcmalloc/latest/bcmalloc/struct.Bcmalloc.html#method.trim).
- A region backed up front with `Commit::Populate` or `Commit::Lock` keeps every page and gives nothing back. So does any region once the kernel refuses to take pages back, as it does for memory the program has locked, for example with `mlockall`.
- It builds for 64-bit Linux and macOS. Other Unix systems are untested, and other systems, Windows among them, are refused at compile time.
- The minimum supported Rust version is 1.88. The `allocator-api` feature needs 1.100, where the standard library's `Allocator` trait became stable.

## Testing

The lock-free code is checked with loom, the unsafe code with Miri under strict provenance, and the allocator and its chunk bitmap with fuzz targets that compare them against reference models. Line coverage of the unit tests is reported to Codecov. `just` has a recipe for each, and `just --list` shows them all.

## LLM Disclosure

bcmalloc's implementation and design was aided by LLMs (namely Claude Opus 5.5 and OpenAI's Sol 6.1).

## License

Licensed under the Apache License, Version 2.0.

[`Config`]: https://docs.rs/bcmalloc/latest/bcmalloc/struct.Config.html
[`Mapping`]: https://docs.rs/bcmalloc/latest/bcmalloc/struct.Mapping.html
[`Heap`]: https://docs.rs/bcmalloc/latest/bcmalloc/struct.Heap.html
