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
| `huge_pages` | on      | Backs the region with transparent huge pages on Linux. macOS has none. |
| `numa`       | on      | Splits the region between NUMA nodes on Linux machines with several.   |

If the operating system refuses the default reservation, the global allocator retries with half the capacity down to 256 MiB.

## Benchmarks

The suite has 43 workloads in eight groups, including producer and consumer rings, Larson, false sharing, standard collections, resizing and large blocks. Every workload runs in a fresh process, three times per allocator in a rotating order, and the best run counts. The tables show the geometric mean of each group relative to mimalloc, so above 1.00 is better for speed and for memory alike. Run `just bench` to reproduce them.

On an Apple M4 Max with macOS 26.5 and Rust 1.100, where the system allocator is libmalloc:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.51   | 3.17     | 1.00     | 0.50     | 1.55     |
| speed, bulk            | 0.28   | 1.35     | 1.00     | 0.40     | 0.73     |
| speed, churn           | 0.25   | 2.36     | 1.00     | 1.36     | 1.59     |
| speed, sharing         | 0.56   | 2.94     | 1.00     | 2.12     | 1.56     |
| speed, false sharing   | 0.47   | 0.99     | 1.00     | 0.71     | 1.00     |
| speed, collections     | 0.67   | 1.22     | 1.00     | 0.65     | 1.12     |
| speed, resizing        | 0.89   | 1.04     | 1.00     | 0.99     | 0.88     |
| speed, large           | 0.99   | 1.16     | 1.00     | 0.15     | 0.12     |
| speed, all workloads   | 0.49   | 1.95     | 1.00     | 0.82     | 1.15     |
| speed, mean of groups  | 0.52   | 1.60     | 1.00     | 0.67     | 0.88     |
| memory held afterwards | 3.46   | 7.60     | 1.00     | 0.98     | 6.88     |
| peak resident memory   | 1.05   | 1.00     | 1.00     | 0.99     | 1.11     |

On a 32-core AMD EPYC 9354P with Ubuntu 24.04, kernel 7.0 and Rust 1.100, where the system allocator is glibc 2.39. Memory is the resident set there, which is not the macOS footprint, so compare allocators within one table and not the machines with each other.

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.07   | 2.72     | 1.00     | 0.66     | 1.68     |
| speed, bulk            | 0.12   | 1.13     | 1.00     | 0.33     | 0.55     |
| speed, churn           | 0.80   | 2.18     | 1.00     | 1.77     | 1.83     |
| speed, sharing         | 0.32   | 2.21     | 1.00     | 2.17     | 1.08     |
| speed, false sharing   | 0.99   | 1.00     | 1.00     | 0.98     | 1.00     |
| speed, collections     | 0.58   | 1.04     | 1.00     | 0.71     | 0.95     |
| speed, resizing        | 1.02   | 1.10     | 1.00     | 0.60     | 0.90     |
| speed, large           | 0.44   | 1.08     | 1.00     | 0.08     | 0.23     |
| speed, all workloads   | 0.61   | 1.71     | 1.00     | 0.86     | 1.12     |
| speed, mean of groups  | 0.55   | 1.44     | 1.00     | 0.64     | 0.88     |
| memory held afterwards | 33.70  | 7.48     | 1.00     | 0.98     | 1.01     |
| peak resident memory   | 2.20   | 1.08     | 1.00     | 1.85     | 1.50     |

On macOS bcmalloc is the fastest allocator on 37 of the 43 workloads and within 5% of the fastest on four more. On Linux it is the fastest on 25 and within 5% on seven more. jemalloc is ahead on the channels, where a consumer frees blocks it never reuses, and on macOS when one vector grows to millions of elements. On Linux glibc is ahead on single very large blocks and on vector growth, and snmalloc by under 15% on the smallest blocks of a single thread and on rewriting a tree. After a 256 MiB burst is freed, bcmalloc holds under 5 MiB on both machines, where mimalloc and jemalloc hold about 260. DESIGN.md explains the design and has the charts.

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
