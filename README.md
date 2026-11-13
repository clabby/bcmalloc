# bcmalloc

<img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"> <img src="https://img.shields.io/badge/rust-1.100%2B-orange.svg" alt="Rust 1.100+"> <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg" alt="Platforms">

A global allocator for long-lived, highly concurrent Rust programs. It reserves one region of address space up front and serves small blocks from per-thread stacks of free pointers, without atomic operations and without touching the blocks. Blocks freed by another thread go home in batched messages, one compare-and-swap per batch. Memory freed by one size class is reused by every other, memory that stays idle goes back to the operating system, and the capacity you configure is a hard limit.

Usage is one line. The region is reserved the first time anything allocates.

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

There are two sets of benchmarks. The development suite has 51 synthetic workloads in eight groups, from single-thread loops to producer and consumer rings, and it is what changes are tuned against. The held-out set is never tuned against: real Rust programs built on serde_json, regex, syn, hyper and tokio, and the C programs of [mimalloc-bench](https://github.com/daanx/mimalloc-bench) run with bcmalloc preloaded. Every Rust workload runs ten times per allocator in a rotating order, each run in a fresh process, and the tables show medians relative to mimalloc, so above 1.00 is better for speed and for memory alike. `just bench`, `just bench-apps` and `just mimalloc-bench` reproduce them.

The development suite on an Apple M4 Max with macOS 26.5 and Rust 1.100, where the system allocator is libmalloc:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.55   | 3.95     | 1.00     | 0.54     | 1.70     |
| speed, bulk            | 0.27   | 1.50     | 1.00     | 0.43     | 0.79     |
| speed, churn           | 0.29   | 2.42     | 1.00     | 1.46     | 1.60     |
| speed, sharing         | 0.55   | 3.03     | 1.00     | 2.18     | 1.60     |
| speed, false sharing   | 0.86   | 1.00     | 1.00     | 0.77     | 1.00     |
| speed, collections     | 0.71   | 1.24     | 1.00     | 0.67     | 1.20     |
| speed, resizing        | 1.00   | 1.02     | 1.00     | 0.92     | 0.85     |
| speed, large           | 0.98   | 1.06     | 1.00     | 0.14     | 0.11     |
| speed, all workloads   | 0.52   | 2.07     | 1.00     | 0.88     | 1.22     |
| speed, mean of groups  | 0.59   | 1.66     | 1.00     | 0.69     | 0.89     |
| speed, cold            | 0.52   | 2.04     | 1.00     | 0.87     | 1.23     |
| latency, p99           | 0.53   | 3.21     | 1.00     | 2.38     | 1.66     |
| memory held afterwards | 2.29   | 3.11     | 1.00     | 0.96     | 3.90     |
| peak resident memory   | 0.98   | 0.99     | 1.00     | 0.99     | 1.09     |

On a 32-core AMD EPYC 9354P with Ubuntu 24.04, kernel 7.0 and Rust 1.100, where the system allocator is glibc 2.39:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.05   | 3.11     | 1.00     | 0.64     | 1.73     |
| speed, bulk            | 0.13   | 1.15     | 1.00     | 0.33     | 0.57     |
| speed, churn           | 0.82   | 2.21     | 1.00     | 1.65     | 1.70     |
| speed, sharing         | 0.32   | 2.27     | 1.00     | 2.16     | 1.08     |
| speed, false sharing   | 0.89   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.61   | 1.01     | 1.00     | 0.72     | 0.94     |
| speed, resizing        | 1.02   | 1.06     | 1.00     | 0.60     | 0.90     |
| speed, large           | 0.44   | 1.09     | 1.00     | 0.08     | 0.23     |
| speed, all workloads   | 0.62   | 1.76     | 1.00     | 0.88     | 1.14     |
| speed, mean of groups  | 0.55   | 1.46     | 1.00     | 0.63     | 0.88     |
| speed, cold            | 0.62   | 1.76     | 1.00     | 0.86     | 1.08     |
| latency, p99           | 0.94   | 3.04     | 1.00     | 1.67     | 2.00     |
| memory held afterwards | 11.88  | 3.12     | 1.00     | 1.06     | 1.10     |
| peak resident memory   | 2.20   | 1.11     | 1.00     | 1.86     | 1.50     |

The held-out programs:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc | tcmalloc |
| --- | --- | --- | --- | --- | --- | --- |
| speed, Rust programs, macOS | 0.91 | 1.03 | 1.00 | 0.92 | 1.00 | - |
| speed, Rust programs, Linux | 0.82 | 1.11 | 1.00 | 1.04 | 1.04 | - |
| speed, mimalloc-bench, Linux | 0.74 | 0.99 | 1.00 | 0.86 | 0.96 | 0.64 |
| memory, mimalloc-bench, Linux | 1.06 | 0.51 | 1.00 | 0.83 | 0.84 | 0.77 |

On the synthetic suite bcmalloc leads by a wide margin on both machines. On the held-out programs the margin is small. It is the fastest on the Rust programs on both machines, by 3% over the next allocator on macOS and 7% on Linux, and it ties mimalloc for speed on mimalloc-bench while holding about twice its memory at peak. DESIGN.md has the per-program tables and explains the design.

## Limits

- Alignments above 64 KiB are refused, and the allocation fails.
- A thread that finds all `max_heaps` heaps taken fails its allocations, which aborts the program.
- The capacity is a hard limit. Nothing grows the region after start-up.
- Linux and macOS are supported. Windows is not.
- The standard library's `Allocator` trait needs Rust 1.100 or later, which is the minimum supported version.

## Testing

The lock-free code is checked with loom, the unsafe code with Miri under strict provenance, and the allocator and its chunk bitmap with fuzz targets. `just` has a recipe for each, and `just --list` shows them all.

## LLM Disclosure

bcmalloc's implementation and design was aided by LLMs (namely Claude Opus 5.5 and OpenAI's Sol 6.1).

## License

Licensed under the Apache License, Version 2.0.

[`Config`]: crate::Config
[`Mapping`]: crate::Mapping
[`Heap`]: crate::Heap
