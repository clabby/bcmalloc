# bcmalloc

<img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"> <img src="https://img.shields.io/badge/rust-1.100%2B-orange.svg" alt="Rust 1.100+"> <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg" alt="Platforms">

A global allocator for long-lived, highly concurrent Rust programs. It reserves one region of address space up front and serves small blocks from thread-local heaps without atomic operations. A free from another thread costs one compare-and-swap. Memory freed by one size class is reused by every other, memory that stays idle goes back to the operating system, and the capacity you configure is a hard limit.

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

| Option       | Default | What it does                                                      |
| ------------ | ------- | ----------------------------------------------------------------- |
| `capacity`   | 64 GiB  | Size of the region. It is address space until memory is touched.  |
| `max_heaps`  | 4096    | How many threads can allocate at once.                            |
| `commit`     | `Lazy`  | `Populate` backs every page now and `Lock` also pins them in RAM. |
| `huge_pages` | off     | Asks Linux for transparent huge pages. macOS ignores it.          |

If the operating system refuses the default reservation, the global allocator retries with half the capacity down to 256 MiB.

## Benchmarks

The suite has 43 workloads in eight groups, including producer and consumer rings, Larson, false sharing, standard collections, resizing and large blocks. Every workload runs in a fresh process, three times per allocator in a rotating order, and the best run counts. The table shows the geometric mean of each group relative to mimalloc, so above 1.00 is better for speed and for memory alike. The numbers come from an Apple M4 Max with macOS 26.5 and Rust 1.100, and the system allocator is macOS libmalloc. Run ~just bench~ to reproduce them.

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.47   | 2.18     | 1.00     | 0.65     | 0.73     |
| speed, bulk            | 0.25   | 1.45     | 1.00     | 0.39     | 0.62     |
| speed, churn           | 0.23   | 2.25     | 1.00     | 1.28     | 1.32     |
| speed, sharing         | 0.54   | 1.91     | 1.00     | 2.00     | 1.43     |
| speed, false sharing   | 0.49   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.61   | 1.21     | 1.00     | 0.66     | 1.09     |
| speed, resizing        | 0.98   | 0.92     | 1.00     | 0.99     | 0.88     |
| speed, large           | 0.94   | 1.16     | 1.00     | 0.15     | 0.14     |
| speed, all workloads   | 0.46   | 1.64     | 1.00     | 0.86     | 0.92     |
| speed, mean of groups  | 0.50   | 1.43     | 1.00     | 0.71     | 0.76     |
| memory held afterwards | 3.45   | 9.59     | 1.00     | 1.07     | 6.88     |
| peak resident memory   | 1.12   | 1.10     | 1.00     | 1.03     | 1.14     |

bcmalloc is the fastest allocator on 32 of the 43 workloads and within 5% of the fastest on three more. jemalloc is ahead when threads free blocks that other threads allocated, which covers the rings, the channels and the shared table, and when a single vector grows to millions of elements. mimalloc is ahead on random reallocation. After a 256 MiB burst is freed, bcmalloc holds 2.8 MiB, where mimalloc holds 261 and jemalloc 222. DESIGN.md has the full results, the eight losses, and the directions we would take next.

The same suite has also run on Linux, on a 32-core AMD EPYC 9354P with simultaneous multithreading off, Ubuntu 24.04 with kernel 7.0, and Rust 1.100. The system allocator there is glibc 2.39. Memory is measured as the resident set, which is not the macOS footprint, so the two tables compare allocators on one machine and not the machines with each other.

| Linux, compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ----------------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair                   | 1.11   | 1.47     | 1.00     | 0.83     | 0.79     |
| speed, bulk                   | 0.13   | 0.65     | 1.00     | 0.32     | 0.51     |
| speed, churn                  | 0.79   | 1.83     | 1.00     | 1.57     | 1.78     |
| speed, sharing                | 0.32   | 1.00     | 1.00     | 2.03     | 1.07     |
| speed, false sharing          | 0.88   | 1.00     | 1.00     | 0.99     | 1.00     |
| speed, collections            | 0.58   | 0.93     | 1.00     | 0.71     | 0.95     |
| speed, resizing               | 1.03   | 0.95     | 1.00     | 0.59     | 0.89     |
| speed, large                  | 0.44   | 0.97     | 1.00     | 0.07     | 0.22     |
| speed, all workloads          | 0.61   | 1.17     | 1.00     | 0.86     | 0.93     |
| speed, mean of groups         | 0.55   | 1.05     | 1.00     | 0.63     | 0.78     |
| memory held afterwards        | 33.70  | 14.93    | 1.00     | 0.98     | 1.01     |
| peak resident memory          | 2.24   | 2.11     | 1.00     | 1.87     | 1.97     |

On Linux bcmalloc is the fastest on 11 of the 41 speed workloads and within 5% of the fastest on five more. The lead over mimalloc is smaller than on macOS, at 1.17 over all workloads. snmalloc is ahead when one thread allocates and frees blocks up to 16 KiB, mimalloc on bulk frees, and jemalloc again on the rings, the channels and the shared table. After the 256 MiB burst bcmalloc holds 1.2 MiB, where mimalloc holds 260 and glibc 0.2.

## Limits

- Alignments above 64 KiB are refused, and the allocation fails.
- A thread that finds all `max_heaps` heaps taken fails its allocations, which aborts the program.
- The capacity is a hard limit. Nothing grows the region after start-up.
- Linux and macOS are supported. Windows is not. On Linux, the tests and benchmarks have run on one x86-64 machine.
- The standard library's `Allocator` trait needs Rust 1.100 or later, which is the minimum supported version.

## Testing

The lock-free code is checked with loom, the unsafe code with Miri under strict provenance, and the allocator and its chunk bitmap with fuzz targets. `just` has a recipe for each, and `just --list` shows them all.

## License

Licensed under the Apache License, Version 2.0.

[`Config`]: crate::Config
[`Mapping`]: crate::Mapping
[`Heap`]: crate::Heap
