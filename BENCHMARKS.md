# Benchmarks

bcmalloc against the system allocator, mimalloc, jemalloc and snmalloc on three sets of programs. The development suite of 51 synthetic workloads is what the allocator is tuned against. The held-out Rust programs and the C programs of [mimalloc-bench](https://github.com/daanx/mimalloc-bench) are never tuned against, and show whether the gains carry over to code written for other purposes.

![For every suite and every other allocator, one dot per workload showing how much faster bcmalloc is](assets/benchmarks.svg)

Every number on this page, and the figure, comes from [benchmarks.json](benchmarks.json). The benchmark commands record their results there, and `just bench-render` renders this page's tables, the table in the README and the figure from it. CI fails when they are out of date.

## Method

- **Machines.** An Apple M4 Max, where the system allocator is libmalloc, and an idle 32-core AMD EPYC 9354P with Ubuntu 24.04, glibc 2.39, transparent huge pages set to `madvise` and CPU boost on. The [machines table](#machines) gives the exact systems.
- **Versions.** The Rust suites use the latest published crates, mimalloc 0.1.52 (mimalloc 3.3.2), tikv-jemallocator 0.7 (jemalloc 5.3.1) and snmalloc-rs 0.7.6. mimalloc-bench pins mimalloc 1.8.2, so it was rebuilt with mimalloc 3.6.1, the latest release, next to jemalloc 5.4.0 and snmalloc 0.7.6. Rust is 1.100.
- **Runs.** Every Rust workload runs ten times per allocator, in a fresh process each time, with the allocators taking turns in a rotating order. The tables show medians, and bold marks the best median and every median whose 95% bootstrap interval overlaps it. The cold rows are the first pass of each workload and the others the steady state. Each mimalloc-bench program ran ten times per allocator through `scripts/mbrun.py`, which runs the suite's own binaries with their arguments but times them in nanoseconds, rotates the allocators within every run, and repeats a program that finishes in a fraction of a second several times within one sample, so that a timer tick does not decide it. Its table shows the median time of one repetition and the median peak resident memory. The programs that report their own throughput, larson, rptest, xmalloc-test, glibc-thread and redis, are scored by it and converted to a time as mimalloc-bench's `bench.sh` converts it.
- **Reproducing.** `just bench`, `just bench-apps` and `just mbrun` run the three sets, and `just mimalloc-bench` runs mimalloc-bench's own driver. `just bench --save PATH` keeps the raw samples, `just bench --load PATH` prints their report again, and `just bench-compare OLD NEW` compares two of them.

## Summary

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

Each figure is mimalloc's time or peak memory divided by the other allocator's, so above 1.00 means faster or smaller. It is the geometric mean over the suite's speed workloads, or over its programs for memory.

bcmalloc is the fastest allocator on both development suites, by a wide margin in most groups, though jemalloc is slightly ahead on the sharing group on Linux and mimalloc on the resizing group on macOS. It is the fastest on the Rust programs on Linux. On macOS it is level with snmalloc, which is faster on the rayon pipeline there, and mimalloc is faster on json. On mimalloc-bench it is the fastest of the four, 2% ahead of mimalloc, and holds two thirds of mimalloc's peak memory, the least of the four. It is not the fastest on every program there. mimalloc is faster on sh6bench by 12%, mstress by 8%, glibc-simple by 5%, sh8bench by 4%, alloc-testN by 2%, and leanN and malloc-large by 1%. On sh6bench the gap closes when every heap takes huge pages, but that costs small programs several times their memory, so bcmalloc keeps small threads on small pages. snmalloc is faster on glibc-thread by 6%, redis by 3%, and cfrac and espresso by 1%.

## Machines

<!-- generated:machines -->
| Suite                    | Machine                                                           | Runs | Date       |
| ------------------------ | ----------------------------------------------------------------- | ---- | ---------- |
| development suite, macOS | Apple M4 Max, macOS 27.0.1                                        | 10   | 2026-11-14 |
| development suite, Linux | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 6.8.0 | 10   | 2026-11-14 |
| Rust programs, macOS     | Apple M4 Max, macOS 27.0.1                                        | 10   | 2026-11-14 |
| Rust programs, Linux     | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 6.8.0 | 10   | 2026-11-14 |
| mimalloc-bench, Linux    | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 6.8.0 | 10   | 2026-11-14 |
<!-- /generated:machines -->

## mimalloc-bench

The geometric means, then the wall time in seconds and the peak resident memory in MiB of every program, as `time / memory`, with the best of each in bold. lua is left out, because its sources are not part of the suite as built. mimalloc-bench's own driver, `bench.sh` with three runs per allocator and its timer of 10 ms, gives the same picture from the same build: bcmalloc at 1.02 of mimalloc's speed and 1.48 for memory, snmalloc at 0.94 and 1.40, jemalloc at 0.82 and 1.38.

<!-- generated:mimalloc-bench:linux -->
| Compared with mimalloc | bcmalloc | mimalloc | snmalloc | jemalloc |
| ---------------------- | -------- | -------- | -------- | -------- |
| speed                  | 1.02     | 1.00     | 0.94     | 0.82     |
| peak resident memory   | 1.49     | 1.00     | 1.40     | 1.37     |

| Program        | bcmalloc              | mimalloc              | snmalloc            | jemalloc           |
| -------------- | --------------------- | --------------------- | ------------------- | ------------------ |
| cfrac          | 3.190 / **3.2**       | 3.249 / 4.7           | **3.154** / 3.5     | 3.286 / 5.2        |
| espresso       | 3.450 / **3.8**       | 3.482 / 10.4          | **3.425** / 4.5     | 3.545 / 6.2        |
| barnes         | **2.176** / 63.5      | 2.214 / 67.5          | 2.263 / **58.0**    | 2.264 / 59.8       |
| redis          | 3.240 / 11.2          | 3.510 / 20.1          | **3.140** / **8.0** | 3.750 / 10.0       |
| larsonN        | **1.031** / 350.6     | 1.175 / 306.9         | 1.216 / 415.0       | 1.276 / **260.4**  |
| larsonN-sized  | **0.988** / 355.0     | 1.136 / 308.6         | 1.201 / 413.6       | 1.149 / **271.9**  |
| mstressN       | 0.691 / 606.2         | **0.637** / 1035.0    | 0.816 / 812.7       | 1.243 / **532.1**  |
| rptestN        | **0.195** / 228.1     | 0.226 / 225.0         | 0.214 / 169.4       | 0.508 / **159.3**  |
| gs             | **0.229** / 39.9      | 0.231 / 55.5          | 0.249 / **36.0**    | 0.246 / 37.0       |
| alloc-test1    | **2.526** / 16.2      | 2.527 / 16.2          | 2.595 / 13.8        | 2.584 / **12.9**   |
| alloc-testN    | 2.344 / **15.8**      | **2.296** / 60.5      | 2.309 / 16.0        | 2.324 / 16.2       |
| sh6benchN      | 0.060 / 281.1         | **0.054** / **226.4** | 0.085 / 275.6       | 0.095 / 226.8      |
| sh8benchN      | 0.147 / 273.9         | **0.141** / 510.5     | 0.188 / 206.4       | 0.274 / **170.7**  |
| xmalloc-testN  | **0.161** / 83.9      | 0.217 / **66.3**      | 0.177 / 126.6       | 0.166 / 147.7      |
| cache-scratch1 | **1.059** / **4.1**   | **1.059** / 6.2       | **1.059** / 4.2     | 1.060 / 4.5        |
| cache-scratchN | **0.037** / 4.0       | 0.037 / 6.2           | 0.037 / **3.8**     | 0.038 / 4.5        |
| cache-thrash1  | **1.059** / **4.0**   | **1.059** / 6.2       | **1.059** / 4.2     | 1.060 / 4.5        |
| cache-thrashN  | **0.037** / **4.0**   | 0.037 / 6.2           | 0.037 / **4.0**     | 0.038 / 4.5        |
| glibc-simple   | 1.670 / **2.0**       | **1.586** / 4.2       | 1.621 / 2.5         | 1.922 / 4.8        |
| glibc-thread   | 0.204 / **10.4**      | 0.197 / 88.4          | **0.193** / 21.0    | 0.205 / 14.5       |
| rocksdb        | **4.170** / 102.9     | 4.191 / 141.9         | 4.373 / 107.0       | 4.358 / **102.7**  |
| leanN          | 9.170 / 261.6         | **9.107** / 339.3     | 9.125 / 241.4       | 9.287 / **219.6**  |
| linux          | **9.082** / 171.9     | 9.625 / 187.2         | 9.689 / **165.8**   | 11.010 / 167.3     |
| malloc-large   | 1.401 / 579.6         | **1.386** / 588.1     | 3.557 / 658.0       | 14.530 / **429.6** |
| mleak10        | 0.153 / **2.0**       | 0.154 / 4.2           | **0.153** / 2.5     | 0.169 / 4.7        |
| mleak100       | 1.511 / **2.0**       | **1.508** / 4.2       | 1.526 / 2.5         | 1.634 / 4.5        |
| rbstress1      | **2.803** / **118.3** | 2.833 / 139.1         | 2.833 / 119.8       | 2.808 / 119.0      |
| rbstressN      | **3.013** / **159.1** | 3.038 / 189.9         | 3.053 / 209.4       | 3.036 / 198.0      |
<!-- /generated:mimalloc-bench:linux -->

## Rust programs

Each program is a crate written for its own purpose and driven with a fixed input. They are built on serde_json, regex, syn, a hyper server with eight clients and one with 256, tokio channels, a fjall key-value store and a rayon pipeline.

### macOS

<!-- generated:apps:macos:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.94   | 1.03     | 1.00     | 0.96     | 1.04     |
| peak resident memory   | 1.18   | 0.97     | 1.00     | 1.12     | 1.11     |
| speed, cold            | 0.94   | 1.04     | 1.00     | 0.96     | 1.07     |
| latency, p99           | 1.00   | 1.04     | 1.00     | 1.02     | 1.03     |
<!-- /generated:apps:macos:summary -->

<!-- generated:apps:macos -->
<details>
<summary>Every workload on macOS</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### apps

| Workload                | system    | bcmalloc     | mimalloc   | jemalloc     | snmalloc      |
| ----------------------- | --------- | ------------ | ---------- | ------------ | ------------- |
| json (MB/s)             | 271.5     | 310.6        | **322.4**  | 265.2        | **314.2**     |
| regex (MB/s)            | 1460.0    | **1538.0**   | **1532.0** | 1458.0       | **1534.0**    |
| syn (MB/s)              | 16.8      | **21.6**     | **20.6**   | 16.6         | 20.1          |
| http (kreq/s)           | **225.9** | **229.7**    | **233.6**  | **234.4**    | **232.1**     |
| tokio channels (Mmsg/s) | 4.03      | **4.51**     | 4.10       | 4.38         | 4.41          |
| kv store (ops/s)        | 244400.0  | **246200.0** | 246000.0   | **246900.0** | **249500.0**  |
| http many (req/s)       | 148600.0  | **151800.0** | 148300.0   | **150600.0** | **151500.0**  |
| rayon (records/s)       | 2570000.0 | 2985000.0    | 2680000.0  | 2717000.0    | **3484000.0** |

### apps, cold

| Workload                | system    | bcmalloc     | mimalloc     | jemalloc     | snmalloc      |
| ----------------------- | --------- | ------------ | ------------ | ------------ | ------------- |
| json (MB/s)             | 271.9     | **311.1**    | **319.8**    | 264.7        | **312.9**     |
| regex (MB/s)            | 1456.0    | **1540.0**   | **1532.0**   | **1461.0**   | **1536.0**    |
| syn (MB/s)              | 16.4      | **21.5**     | 20.5         | 16.5         | 20.0          |
| http (kreq/s)           | **222.4** | **236.3**    | **228.5**    | **228.9**    | **230.9**     |
| tokio channels (Mmsg/s) | 4.02      | **4.48**     | 4.08         | **4.43**     | **4.42**      |
| kv store (ops/s)        | 243500.0  | **248400.0** | **244400.0** | 239800.0     | **248800.0**  |
| http many (req/s)       | 147800.0  | **150500.0** | 147500.0     | **148700.0** | **151000.0**  |
| rayon (records/s)       | 2356000.0 | 2741000.0    | 2402000.0    | 2493000.0    | **3691000.0** |

### apps, latency (p50 / p99 / p999)

| Workload       | system         | bcmalloc       | mimalloc       | jemalloc       | snmalloc       |
| -------------- | -------------- | -------------- | -------------- | -------------- | -------------- |
| http many (us) | - / 2138.0 / - | - / 2050.0 / - | - / 2133.0 / - | - / 2081.0 / - | - / 2070.0 / - |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.94   | 1.03     | 1.00     | 0.96     | 1.04     |
| peak resident memory   | 1.18   | 0.97     | 1.00     | 1.12     | 1.11     |
| speed, cold            | 0.94   | 1.04     | 1.00     | 0.96     | 1.07     |
| latency, p99           | 1.00   | 1.04     | 1.00     | 1.02     | 1.03     |

</details>
<!-- /generated:apps:macos -->

### Linux

<!-- generated:apps:linux:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.72   | 1.13     | 1.00     | 1.03     | 1.05     |
| peak resident memory   | 1.93   | 1.52     | 1.00     | 1.49     | 1.48     |
| speed, cold            | 0.75   | 1.14     | 1.00     | 1.02     | 1.04     |
| latency, p99           | 0.43   | 1.13     | 1.00     | 1.05     | 1.07     |
<!-- /generated:apps:linux:summary -->

<!-- generated:apps:linux -->
<details>
<summary>Every workload on Linux</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### apps

| Workload                | system      | bcmalloc      | mimalloc    | jemalloc    | snmalloc    |
| ----------------------- | ----------- | ------------- | ----------- | ----------- | ----------- |
| json (MB/s)             | 127.4       | **190.6**     | 178.4       | 167.6       | 176.4       |
| regex (MB/s)            | 989.3       | **1051.0**    | 1036.0      | 1028.0      | 1045.0      |
| syn (MB/s)              | 7.11        | **10.6**      | 9.85        | 9.05        | 9.67        |
| http (kreq/s)           | 100.7       | **120.9**     | 114.2       | 118.8       | 117.3       |
| tokio channels (Mmsg/s) | 1.21        | 1.88          | 1.45        | **2.00**    | 1.78        |
| kv store (ops/s)        | **60990.0** | **62920.0**   | **62480.0** | **62130.0** | **63040.0** |
| http many (req/s)       | 140100.0    | **324200.0**  | 293300.0    | 304200.0    | 309200.0    |
| rayon (records/s)       | 690500.0    | **2344000.0** | 1538000.0   | 1533000.0   | 1773000.0   |

### apps, cold

| Workload                | system   | bcmalloc      | mimalloc    | jemalloc    | snmalloc    |
| ----------------------- | -------- | ------------- | ----------- | ----------- | ----------- |
| json (MB/s)             | 124.2    | **189.7**     | 178.3       | 163.0       | 171.7       |
| regex (MB/s)            | 980.8    | **1049.0**    | 1039.0      | 1019.0      | 1036.0      |
| syn (MB/s)              | 6.96     | **10.5**      | 9.72        | 8.84        | 9.41        |
| http (kreq/s)           | 100.5    | **120.7**     | 113.5       | 119.3       | 117.7       |
| tokio channels (Mmsg/s) | 1.22     | 1.87          | 1.46        | **2.02**    | 1.77        |
| kv store (ops/s)        | 56760.0  | **62840.0**   | **61370.0** | **61220.0** | **60950.0** |
| http many (req/s)       | 141300.0 | **323300.0**  | 288100.0    | 304900.0    | 307800.0    |
| rayon (records/s)       | 944900.0 | **2303000.0** | 1488000.0   | 1443000.0   | 1695000.0   |

### apps, latency (p50 / p99 / p999)

| Workload       | system         | bcmalloc       | mimalloc       | jemalloc       | snmalloc       |
| -------------- | -------------- | -------------- | -------------- | -------------- | -------------- |
| http many (us) | - / 3454.0 / - | - / 1334.0 / - | - / 1502.0 / - | - / 1431.0 / - | - / 1410.0 / - |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.72   | 1.13     | 1.00     | 1.03     | 1.05     |
| peak resident memory   | 1.93   | 1.52     | 1.00     | 1.49     | 1.48     |
| speed, cold            | 0.75   | 1.14     | 1.00     | 1.02     | 1.04     |
| latency, p99           | 0.43   | 1.13     | 1.00     | 1.05     | 1.07     |

</details>
<!-- /generated:apps:linux -->

## Development suite

Eight groups of synthetic workloads, from single-thread loops to producer and consumer rings, Larson, false sharing, standard collections, resizing, large blocks, latency percentiles and memory after a burst.

### macOS

<!-- generated:dev:macos:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.56   | 4.01     | 1.00     | 0.55     | 1.74     |
| speed, bulk            | 0.30   | 1.41     | 1.00     | 0.42     | 0.83     |
| speed, churn           | 0.30   | 2.61     | 1.00     | 1.47     | 1.59     |
| speed, sharing         | 0.54   | 2.84     | 1.00     | 2.06     | 1.56     |
| speed, false sharing   | 0.86   | 1.01     | 1.00     | 0.66     | 1.00     |
| speed, collections     | 0.71   | 1.33     | 1.00     | 0.66     | 1.19     |
| speed, resizing        | 0.89   | 0.93     | 1.00     | 0.93     | 0.90     |
| speed, large           | 0.97   | 1.07     | 1.00     | 0.14     | 0.15     |
| speed, all workloads   | 0.52   | 2.09     | 1.00     | 0.86     | 1.24     |
| memory held afterwards | 2.36   | 3.48     | 1.00     | 0.96     | 3.91     |
| peak resident memory   | 0.99   | 0.97     | 1.00     | 0.98     | 1.08     |
| speed, mean of groups  | 0.59   | 1.65     | 1.00     | 0.67     | 0.93     |
| speed, cold            | 0.51   | 2.04     | 1.00     | 0.86     | 1.25     |
| latency, p99           | 0.53   | 3.41     | 1.00     | 2.41     | 1.73     |
<!-- /generated:dev:macos:summary -->

<!-- generated:dev:macos -->
<details>
<summary>Every workload on macOS</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### pair

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| --------------------- | ------ | --------- | -------- | -------- | -------- |
| pair 16 B (Mops/s)    | 152.8  | **468.8** | 225.0    | 143.6    | 398.7    |
| pair 64 B (Mops/s)    | 138.2  | **400.4** | 220.5    | 143.1    | 388.0    |
| pair 256 B (Mops/s)   | 129.0  | **420.9** | 182.4    | 141.0    | 343.7    |
| pair 1 KiB (Mops/s)   | 54.4   | **388.6** | 149.5    | 134.4    | 286.4    |
| pair 4 KiB (Mops/s)   | 72.3   | **393.5** | 93.8     | 112.0    | 161.1    |
| pair 16 KiB (Mops/s)  | 61.2   | **366.9** | 89.1     | 55.4     | 166.2    |
| pair 64 KiB (Mops/s)  | 14.4   | **381.0** | 87.6     | 11.5     | 162.6    |
| pair 256 KiB (Mops/s) | 14.1   | **236.7** | 89.3     | 7.75     | 39.5     |
| pair 1 MiB (Mops/s)   | 14.3   | **237.7** | 4.04     | 7.84     | 19.6     |

### pair, cold

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| --------------------- | ------ | --------- | -------- | -------- | -------- |
| pair 16 B (Mops/s)    | 153.1  | **461.8** | 225.1    | 143.6    | 398.9    |
| pair 64 B (Mops/s)    | 138.0  | **401.2** | 220.6    | 143.3    | 387.7    |
| pair 256 B (Mops/s)   | 128.2  | **416.3** | 182.0    | 141.0    | 345.5    |
| pair 1 KiB (Mops/s)   | 54.2   | **388.8** | 149.6    | 134.4    | 286.5    |
| pair 4 KiB (Mops/s)   | 72.5   | **387.0** | 94.6     | 112.0    | 161.8    |
| pair 16 KiB (Mops/s)  | 60.9   | **367.8** | 88.7     | 54.9     | 165.8    |
| pair 64 KiB (Mops/s)  | 14.4   | **372.8** | 88.8     | 11.4     | 162.8    |
| pair 256 KiB (Mops/s) | 14.2   | **234.6** | 88.5     | 7.56     | 39.3     |
| pair 1 MiB (Mops/s)   | 14.1   | **235.7** | 4.02     | 7.81     | 19.7     |

### bulk

| Workload                     | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | -------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 98.3   | **311.7** | 173.8    | 67.0     | 218.7    |
| bulk 32 B shuffled (Mops/s)  | 49.5   | **190.4** | 182.6    | 61.1     | 171.1    |
| bulk 512 B shuffled (Mops/s) | 11.8   | **99.6**  | 66.7     | 37.3     | 32.9     |

### bulk, cold

| Workload                     | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | -------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 85.6   | **253.9** | 156.9    | 64.4     | 199.1    |
| bulk 32 B shuffled (Mops/s)  | 47.5   | **173.8** | 164.4    | 58.6     | 158.7    |
| bulk 512 B shuffled (Mops/s) | 11.2   | **58.5**  | 47.9     | 26.9     | 28.4     |

### churn

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------------------- | ------ | ---------- | -------- | -------- | -------- |
| churn small (Mops/s)                | 48.4   | **234.5**  | 97.9     | 123.8    | 214.6    |
| churn medium (Mops/s)               | 25.9   | **233.2**  | 58.7     | 110.8    | 119.9    |
| churn mixed (Mops/s)                | 35.7   | **229.9**  | 83.8     | 114.2    | 128.4    |
| churn skewed (Mops/s)               | 49.1   | **187.3**  | 154.4    | 128.8    | 175.3    |
| churn mixed 4 threads (Mops/s)      | 39.7   | **894.0**  | 307.7    | 444.7    | 427.5    |
| churn mixed 8 threads (Mops/s)      | 68.0   | **1766.0** | 429.9    | 865.8    | 798.2    |
| churn mixed 16 threads (Mops/s)     | 93.5   | **2259.0** | 609.4    | 1163.0   | 1070.0   |
| churn skewed 8 threads (Mops/s)     | 168.8  | **1419.0** | 866.5    | 975.2    | 1312.0   |
| churn mixed oversubscribed (Mops/s) | 124.3  | **2308.0** | 804.0    | 1213.0   | 1134.0   |
| churn large set (Mops/s)            | 7.28   | **67.3**   | 11.6     | 31.5     | 25.3     |
| churn with work (Mops/s)            | 3.17   | **3.43**   | **3.39** | 3.37     | **3.43** |

### churn, cold

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------------------- | ------ | ---------- | -------- | -------- | -------- |
| churn small (Mops/s)                | 48.2   | **234.3**  | 98.0     | 122.5    | 213.8    |
| churn medium (Mops/s)               | 25.9   | **231.8**  | 58.6     | 110.6    | 118.6    |
| churn mixed (Mops/s)                | 35.6   | **229.3**  | 83.5     | 113.9    | 128.8    |
| churn skewed (Mops/s)               | 48.3   | **186.5**  | 154.1    | 128.8    | 174.8    |
| churn mixed 4 threads (Mops/s)      | 39.4   | **883.9**  | 320.4    | 444.0    | 427.2    |
| churn mixed 8 threads (Mops/s)      | 66.2   | **1719.0** | 434.1    | 866.0    | 800.5    |
| churn mixed 16 threads (Mops/s)     | 91.8   | **2127.0** | 642.1    | 1162.0   | 1059.0   |
| churn skewed 8 threads (Mops/s)     | 164.3  | **1419.0** | 848.8    | 976.8    | 1311.0   |
| churn mixed oversubscribed (Mops/s) | 90.5   | **2282.0** | 836.0    | 1217.0   | 1110.0   |
| churn large set (Mops/s)            | 6.63   | **41.7**   | 10.5     | 22.8     | 22.2     |
| churn with work (Mops/s)            | 3.16   | **3.42**   | 3.40     | 3.37     | **3.42** |

### sharing

| Workload                        | system | bcmalloc   | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | ---------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 35.4   | **1643.0** | 668.9    | 884.8     | 730.5    |
| ring 4 threads (Mops/s)         | 112.2  | **479.2**  | 111.9    | 443.6     | 177.1    |
| ring 8 threads (Mops/s)         | 110.1  | 547.4      | 80.6     | **566.5** | 179.3    |
| channel 1 pair (Mops/s)         | 28.9   | **60.3**   | 25.6     | 54.1      | 43.1     |
| channel 4 pairs (Mops/s)        | 47.0   | 113.7      | 59.6     | **131.3** | 119.9    |
| shared table 8 threads (Mops/s) | 22.8   | **97.0**   | 39.6     | 83.0      | 36.2     |
| thread lifecycle (Mops/s)       | 37.3   | **196.5**  | 102.7    | 44.7      | 190.3    |

### sharing, cold

| Workload                        | system | bcmalloc   | mimalloc | jemalloc  | snmalloc  |
| ------------------------------- | ------ | ---------- | -------- | --------- | --------- |
| larson 8 threads (Mops/s)       | 35.5   | **1553.0** | 657.3    | 869.7     | 727.1     |
| ring 4 threads (Mops/s)         | 97.5   | **472.8**  | 108.3    | 439.8     | 177.2     |
| ring 8 threads (Mops/s)         | 109.8  | **546.4**  | 80.2     | **547.9** | 178.1     |
| channel 1 pair (Mops/s)         | 28.7   | **60.3**   | 25.8     | 51.3      | 50.9      |
| channel 4 pairs (Mops/s)        | 45.4   | 113.8      | 55.8     | **134.1** | 117.6     |
| shared table 8 threads (Mops/s) | 19.8   | **97.4**   | 39.3     | 93.0      | 35.7      |
| thread lifecycle (Mops/s)       | 35.6   | **187.5**  | 102.4    | 44.3      | **182.8** |

### false sharing

| Workload                         | system  | bcmalloc    | mimalloc    | jemalloc    | snmalloc    |
| -------------------------------- | ------- | ----------- | ----------- | ----------- | ----------- |
| cache scratch 8 threads (Mops/s) | 26740.0 | **31060.0** | **30820.0** | 13390.0     | **30960.0** |
| cache thrash 8 threads (Mops/s)  | 26560.0 | **31040.0** | **30920.0** | **30890.0** | **30990.0** |

### false sharing, cold

| Workload                         | system  | bcmalloc    | mimalloc    | jemalloc    | snmalloc    |
| -------------------------------- | ------- | ----------- | ----------- | ----------- | ----------- |
| cache scratch 8 threads (Mops/s) | 27890.0 | **31060.0** | 30930.0     | 12630.0     | 30970.0     |
| cache thrash 8 threads (Mops/s)  | 28290.0 | **31040.0** | **30950.0** | **30890.0** | **31020.0** |

### collections

| Workload                        | system | bcmalloc  | mimalloc | jemalloc | snmalloc  |
| ------------------------------- | ------ | --------- | -------- | -------- | --------- |
| hash map (Mops/s)               | 8.37   | **15.8**  | 12.8     | 13.9     | 14.9      |
| btree map (Mops/s)              | 5.64   | **7.02**  | 6.06     | 6.51     | 6.57      |
| strings (Mops/s)                | 7.58   | 8.81      | 8.82     | 7.67     | **8.99**  |
| linked list (Mops/s)            | 92.9   | **182.9** | 149.8    | 66.4     | **192.5** |
| binary trees (Mops/s)           | 75.3   | **204.9** | 120.2    | 56.2     | 157.6     |
| tree rewrite (Mops/s)           | 79.3   | **255.6** | 129.0    | 57.9     | 172.2     |
| binary trees with work (Mops/s) | 54.1   | **91.3**  | 75.1     | 44.0     | 85.8      |

### collections, cold

| Workload                        | system | bcmalloc  | mimalloc | jemalloc | snmalloc  |
| ------------------------------- | ------ | --------- | -------- | -------- | --------- |
| hash map (Mops/s)               | 8.19   | **14.9**  | 12.2     | 13.7     | 14.6      |
| btree map (Mops/s)              | 5.54   | **6.90**  | 5.97     | 6.34     | 6.55      |
| strings (Mops/s)                | 7.59   | 8.78      | 8.80     | 7.65     | **8.95**  |
| linked list (Mops/s)            | 90.5   | **177.0** | 145.9    | 65.9     | **185.1** |
| binary trees (Mops/s)           | 74.3   | **196.1** | 119.3    | 56.0     | 154.8     |
| tree rewrite (Mops/s)           | 79.3   | **255.7** | 129.4    | 57.4     | 171.5     |
| binary trees with work (Mops/s) | 54.4   | **91.3**  | 75.4     | 44.0     | 88.4      |

### resizing

| Workload                | system    | bcmalloc  | mimalloc   | jemalloc  | snmalloc  |
| ----------------------- | --------- | --------- | ---------- | --------- | --------- |
| vec growth (Mops/s)     | 871.9     | 680.4     | 641.7      | **967.1** | 870.0     |
| string growth (Mops/s)  | 1965.0    | 1937.0    | **3031.0** | 2499.0    | 2114.0    |
| many vectors (Mops/s)   | **306.9** | **319.8** | **315.0**  | **309.9** | **313.4** |
| realloc random (Mops/s) | 9.11      | **13.3**  | 12.5       | 7.76      | 8.50      |

### resizing, cold

| Workload                | system | bcmalloc  | mimalloc   | jemalloc  | snmalloc  |
| ----------------------- | ------ | --------- | ---------- | --------- | --------- |
| vec growth (Mops/s)     | 919.9  | 691.9     | 650.3      | **960.3** | 849.5     |
| string growth (Mops/s)  | 1962.0 | 1942.0    | **2998.0** | 2291.0    | 2100.0    |
| many vectors (Mops/s)   | 298.4  | **318.0** | **312.6**  | 305.4     | **313.3** |
| realloc random (Mops/s) | 9.10   | **13.2**  | 12.4       | 7.75      | 8.52      |

### large

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 1.76   | **2.08** | 1.56     | 1.37     | 0.70     |
| large blocks (Mops/s)  | 0.21   | 0.21     | **0.25** | 0.01     | 0.01     |

### large, cold

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 1.72   | **2.04** | 1.50     | 1.31     | 0.68     |
| large blocks (Mops/s)  | 0.17   | 0.17     | **0.21** | 0.01     | 0.01     |

### memory

| Workload              | system | bcmalloc | mimalloc  | jemalloc | snmalloc  |
| --------------------- | ------ | -------- | --------- | -------- | --------- |
| burst retention (MiB) | 22.0   | 7.44     | 260.6     | 266.3    | **5.48**  |
| half freed (MiB)      | 153.4  | 154.1    | **153.2** | 155.9    | 153.6     |
| phase churn (MiB)     | 133.3  | 122.4    | 148.2     | 163.4    | **118.1** |

### latency

| Workload                     | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | -------- | -------- | -------- | -------- |
| latency churn 1 thread (ns)  | 16.3   | **3.25** | 7.16     | 7.81     | 5.21     |
| latency churn 8 threads (ns) | 105.1  | **3.27** | 16.3     | 7.81     | 7.17     |
| latency remote frees (ns)    | 149.7  | 90.5     | 326.2    | **44.3** | 196.0    |

### latency, latency (p50 / p99 / p999)

| Workload                     | system               | bcmalloc            | mimalloc             | jemalloc           | snmalloc             |
| ---------------------------- | -------------------- | ------------------- | -------------------- | ------------------ | -------------------- |
| latency churn 1 thread (ns)  | 13.0 / 16.3 / 18.2   | 1.31 / 3.25 / 4.56  | 4.56 / 7.16 / 9.11   | 3.91 / 7.81 / 9.77 | 3.27 / 5.21 / 8.45   |
| latency churn 8 threads (ns) | 54.7 / 105.1 / 135.4 | 1.95 / 3.27 / 5.20  | 7.16 / 16.3 / 22.5   | 3.91 / 7.81 / 9.77 | 3.91 / 7.17 / 10.4   |
| latency remote frees (ns)    | 29.9 / 149.7 / 369.1 | 1.31 / 90.5 / 379.6 | 7.81 / 326.2 / 583.3 | 7.81 / 44.3 / 71.6 | 6.50 / 196.0 / 840.5 |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.56   | 4.01     | 1.00     | 0.55     | 1.74     |
| speed, bulk            | 0.30   | 1.41     | 1.00     | 0.42     | 0.83     |
| speed, churn           | 0.30   | 2.61     | 1.00     | 1.47     | 1.59     |
| speed, sharing         | 0.54   | 2.84     | 1.00     | 2.06     | 1.56     |
| speed, false sharing   | 0.86   | 1.01     | 1.00     | 0.66     | 1.00     |
| speed, collections     | 0.71   | 1.33     | 1.00     | 0.66     | 1.19     |
| speed, resizing        | 0.89   | 0.93     | 1.00     | 0.93     | 0.90     |
| speed, large           | 0.97   | 1.07     | 1.00     | 0.14     | 0.15     |
| speed, all workloads   | 0.52   | 2.09     | 1.00     | 0.86     | 1.24     |
| memory held afterwards | 2.36   | 3.48     | 1.00     | 0.96     | 3.91     |
| peak resident memory   | 0.99   | 0.97     | 1.00     | 0.98     | 1.08     |
| speed, mean of groups  | 0.59   | 1.65     | 1.00     | 0.67     | 0.93     |
| speed, cold            | 0.51   | 2.04     | 1.00     | 0.86     | 1.25     |
| latency, p99           | 0.53   | 3.41     | 1.00     | 2.41     | 1.73     |

</details>
<!-- /generated:dev:macos -->

### Linux

<!-- generated:dev:linux:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.09   | 3.50     | 1.00     | 0.66     | 1.79     |
| speed, bulk            | 0.14   | 1.14     | 1.00     | 0.36     | 0.61     |
| speed, churn           | 0.83   | 2.28     | 1.00     | 1.68     | 1.73     |
| speed, sharing         | 0.33   | 2.07     | 1.00     | 2.19     | 1.13     |
| speed, false sharing   | 0.92   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.63   | 1.09     | 1.00     | 0.73     | 0.96     |
| speed, resizing        | 1.03   | 1.09     | 1.00     | 0.60     | 0.92     |
| speed, large           | 0.43   | 1.07     | 1.00     | 0.08     | 0.27     |
| speed, all workloads   | 0.64   | 1.82     | 1.00     | 0.89     | 1.18     |
| memory held afterwards | 10.77  | 2.94     | 1.00     | 1.06     | 1.10     |
| peak resident memory   | 2.02   | 1.76     | 1.00     | 1.77     | 1.49     |
| speed, mean of groups  | 0.56   | 1.49     | 1.00     | 0.64     | 0.91     |
| speed, cold            | 0.63   | 1.81     | 1.00     | 0.88     | 1.12     |
| latency, p99           | 0.93   | 3.03     | 1.00     | 1.67     | 1.94     |
<!-- /generated:dev:linux:summary -->

<!-- generated:dev:linux -->
<details>
<summary>Every workload on Linux</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### pair

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| --------------------- | ------ | --------- | -------- | -------- | -------- |
| pair 16 B (Mops/s)    | 130.5  | **341.5** | 266.5    | 251.8    | 314.7    |
| pair 64 B (Mops/s)    | 130.1  | **342.2** | 226.5    | 249.8    | 309.2    |
| pair 256 B (Mops/s)   | 130.7  | **342.3** | 201.2    | 241.6    | 329.6    |
| pair 1 KiB (Mops/s)   | 126.3  | **342.5** | 142.1    | 185.6    | 297.2    |
| pair 4 KiB (Mops/s)   | 55.7   | **342.9** | 58.1     | 154.6    | 194.5    |
| pair 16 KiB (Mops/s)  | 54.9   | **252.6** | 54.3     | 54.3     | 194.2    |
| pair 64 KiB (Mops/s)  | 54.1   | **252.6** | 55.1     | 4.17     | 196.4    |
| pair 256 KiB (Mops/s) | 57.4   | **139.1** | 54.3     | 4.15     | 20.2     |
| pair 1 MiB (Mops/s)   | 57.4   | **135.9** | 4.43     | 4.20     | 9.32     |

### pair, cold

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc  |
| --------------------- | ------ | --------- | -------- | -------- | --------- |
| pair 16 B (Mops/s)    | 130.6  | **342.3** | 280.5    | 251.7    | **328.2** |
| pair 64 B (Mops/s)    | 130.6  | **342.2** | 248.1    | 249.6    | 309.1     |
| pair 256 B (Mops/s)   | 130.6  | **341.4** | 213.7    | 241.4    | 329.3     |
| pair 1 KiB (Mops/s)   | 126.3  | **342.0** | 141.9    | 196.8    | 297.0     |
| pair 4 KiB (Mops/s)   | 55.7   | **342.5** | 58.1     | 157.0    | 194.5     |
| pair 16 KiB (Mops/s)  | 54.9   | **250.6** | 54.4     | 54.3     | 194.0     |
| pair 64 KiB (Mops/s)  | 54.1   | **251.9** | 54.4     | 4.17     | 196.2     |
| pair 256 KiB (Mops/s) | 57.3   | **130.5** | 54.4     | 4.13     | 20.2      |
| pair 1 MiB (Mops/s)   | 56.8   | **130.0** | 4.42     | 4.20     | 9.30      |

### bulk

| Workload                     | system | bcmalloc | mimalloc  | jemalloc | snmalloc |
| ---------------------------- | ------ | -------- | --------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 22.0   | 155.0    | **178.5** | 51.5     | 124.6    |
| bulk 32 B shuffled (Mops/s)  | 8.56   | **93.3** | 77.4      | 43.6     | 57.4     |
| bulk 512 B shuffled (Mops/s) | 6.78   | **52.6** | 37.0      | 10.3     | 15.9     |

### bulk, cold

| Workload                     | system | bcmalloc  | mimalloc  | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | --------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 25.6   | **139.8** | **139.7** | 40.4     | 65.1     |
| bulk 32 B shuffled (Mops/s)  | 9.34   | **87.7**  | 66.2      | 36.0     | 40.2     |
| bulk 512 B shuffled (Mops/s) | 4.71   | **33.6**  | 25.8      | 6.13     | 7.52     |

### churn

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc  |
| ----------------------------------- | ------ | ---------- | -------- | -------- | --------- |
| churn small (Mops/s)                | 61.7   | 120.2      | 68.3     | 108.6    | **125.0** |
| churn medium (Mops/s)               | 36.9   | **118.1**  | 41.2     | 82.2     | 81.0      |
| churn mixed (Mops/s)                | 39.2   | **119.1**  | 57.1     | 86.6     | 88.5      |
| churn skewed (Mops/s)               | 51.6   | **133.1**  | 124.6    | 119.9    | 121.2     |
| churn mixed 4 threads (Mops/s)      | 152.2  | **473.0**  | 125.0    | 336.2    | 342.8     |
| churn mixed 8 threads (Mops/s)      | 303.4  | **945.6**  | 244.2    | 667.5    | 663.7     |
| churn mixed 16 threads (Mops/s)     | 603.0  | **1830.0** | 472.4    | 1313.0   | 1322.0    |
| churn skewed 8 threads (Mops/s)     | 387.3  | **1043.0** | 847.4    | 931.9    | 950.7     |
| churn mixed oversubscribed (Mops/s) | 1045.0 | **3439.0** | 1217.0   | 2435.0   | 2510.0    |
| churn large set (Mops/s)            | 4.81   | **23.3**   | 6.10     | 9.28     | 10.3      |
| churn with work (Mops/s)            | 1.88   | **2.09**   | 2.06     | 1.94     | 1.95      |

### churn, cold

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc  |
| ----------------------------------- | ------ | ---------- | -------- | -------- | --------- |
| churn small (Mops/s)                | 61.1   | 120.5      | 68.1     | 108.2    | **124.6** |
| churn medium (Mops/s)               | 36.8   | **117.2**  | 41.1     | 81.2     | 80.2      |
| churn mixed (Mops/s)                | 39.1   | **118.2**  | 57.0     | 85.9     | 87.3      |
| churn skewed (Mops/s)               | 50.8   | **131.8**  | 123.3    | 118.5    | 119.9     |
| churn mixed 4 threads (Mops/s)      | 150.4  | **464.0**  | 132.8    | 335.3    | 336.6     |
| churn mixed 8 threads (Mops/s)      | 293.5  | **923.9**  | 241.1    | 662.7    | 660.3     |
| churn mixed 16 threads (Mops/s)     | 560.4  | **1741.0** | 461.7    | 1319.0   | 1312.0    |
| churn skewed 8 threads (Mops/s)     | 384.4  | **1027.0** | 829.5    | 926.3    | 945.8     |
| churn mixed oversubscribed (Mops/s) | 873.5  | **3355.0** | 1358.0   | 2413.0   | 2506.0    |
| churn large set (Mops/s)            | 3.73   | **18.4**   | 5.65     | 6.19     | 6.11      |
| churn with work (Mops/s)            | 1.89   | **2.09**   | 2.06     | 1.95     | 1.96      |

### sharing

| Workload                        | system | bcmalloc  | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | --------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 144.4  | **795.8** | 405.8    | 604.4     | 548.9    |
| ring 4 threads (Mops/s)         | 9.79   | 74.6      | 23.7     | **118.0** | 34.2     |
| ring 8 threads (Mops/s)         | 17.5   | 147.6     | 46.8     | **229.2** | 67.7     |
| channel 1 pair (Mops/s)         | 1.24   | 9.42      | 5.45     | **10.8**  | 5.63     |
| channel 4 pairs (Mops/s)        | 4.82   | 33.1      | 20.4     | **38.8**  | 21.9     |
| shared table 8 threads (Mops/s) | 4.27   | **78.6**  | 28.4     | 67.6      | 21.6     |
| thread lifecycle (Mops/s)       | 68.1   | **74.4**  | 68.1     | 50.2      | 67.0     |

### sharing, cold

| Workload                        | system | bcmalloc  | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | --------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 141.8  | **788.9** | 404.9    | 587.4     | 541.2    |
| ring 4 threads (Mops/s)         | 9.77   | 74.3      | 23.3     | **115.9** | 34.1     |
| ring 8 threads (Mops/s)         | 17.2   | 146.7     | 45.9     | **223.4** | 68.6     |
| channel 1 pair (Mops/s)         | 1.26   | 9.52      | 5.39     | **10.8**  | 5.56     |
| channel 4 pairs (Mops/s)        | 4.80   | 33.1      | 20.8     | **38.5**  | 22.0     |
| shared table 8 threads (Mops/s) | 4.25   | **78.2**  | 28.4     | 67.0      | 21.6     |
| thread lifecycle (Mops/s)       | 68.3   | **74.4**  | 67.9     | 49.8      | 66.3     |

### false sharing

| Workload                         | system  | bcmalloc | mimalloc    | jemalloc    | snmalloc    |
| -------------------------------- | ------- | -------- | ----------- | ----------- | ----------- |
| cache scratch 8 threads (Mops/s) | 50520.0 | 59930.0  | **60000.0** | 59960.0     | **59990.0** |
| cache thrash 8 threads (Mops/s)  | 59790.0 | 59930.0  | **60010.0** | **59990.0** | **59990.0** |

### false sharing, cold

| Workload                         | system  | bcmalloc | mimalloc    | jemalloc | snmalloc    |
| -------------------------------- | ------- | -------- | ----------- | -------- | ----------- |
| cache scratch 8 threads (Mops/s) | 42420.0 | 59900.0  | **59970.0** | 59810.0  | **59980.0** |
| cache thrash 8 threads (Mops/s)  | 59780.0 | 59900.0  | **59960.0** | 59790.0  | **59960.0** |

### collections

| Workload                        | system | bcmalloc  | mimalloc  | jemalloc | snmalloc |
| ------------------------------- | ------ | --------- | --------- | -------- | -------- |
| hash map (Mops/s)               | 3.90   | **8.83**  | 7.48      | 6.70     | 7.24     |
| btree map (Mops/s)              | 2.79   | **5.18**  | 4.40      | 4.72     | 4.49     |
| strings (Mops/s)                | 6.04   | **7.18**  | **7.19**  | 6.04     | **7.19** |
| linked list (Mops/s)            | 87.8   | **153.7** | **157.7** | 76.4     | 130.4    |
| binary trees (Mops/s)           | 64.1   | **101.7** | 95.2      | 59.4     | 90.3     |
| tree rewrite (Mops/s)           | 37.3   | **121.5** | 98.5      | 56.1     | 106.9    |
| binary trees with work (Mops/s) | 45.9   | **50.0**  | 47.8      | 36.5     | 41.8     |

### collections, cold

| Workload                        | system | bcmalloc  | mimalloc  | jemalloc | snmalloc |
| ------------------------------- | ------ | --------- | --------- | -------- | -------- |
| hash map (Mops/s)               | 4.30   | **8.69**  | 7.06      | 6.15     | 5.82     |
| btree map (Mops/s)              | 3.54   | **5.11**  | 4.31      | 4.41     | 4.19     |
| strings (Mops/s)                | 5.71   | **7.16**  | **7.13**  | 5.93     | 7.04     |
| linked list (Mops/s)            | 73.7   | **145.2** | **144.3** | 67.2     | 101.2    |
| binary trees (Mops/s)           | 58.7   | **94.7**  | **94.4**  | 56.9     | 86.2     |
| tree rewrite (Mops/s)           | 45.5   | **121.3** | 98.8      | 55.9     | 106.7    |
| binary trees with work (Mops/s) | 45.1   | **50.0**  | 47.7      | 36.2     | 41.0     |

### resizing

| Workload                | system     | bcmalloc | mimalloc | jemalloc | snmalloc |
| ----------------------- | ---------- | -------- | -------- | -------- | -------- |
| vec growth (Mops/s)     | **1820.0** | 1554.0   | 1341.0   | 271.6    | 1024.0   |
| string growth (Mops/s)  | **1876.0** | 1791.0   | 1789.0   | 1813.0   | 1703.0   |
| many vectors (Mops/s)   | **331.5**  | 271.2    | 275.1    | 271.3    | 295.4    |
| realloc random (Mops/s) | 5.38       | **10.2** | 8.33     | 5.29     | 7.49     |

### resizing, cold

| Workload                | system     | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------- | ---------- | ---------- | -------- | -------- | -------- |
| vec growth (Mops/s)     | 1431.0     | **1474.0** | 1286.0   | 265.4    | 825.9    |
| string growth (Mops/s)  | **1810.0** | 1764.0     | 1772.0   | 1755.0   | 1636.0   |
| many vectors (Mops/s)   | **323.8**  | 267.9      | 271.6    | 245.6    | 272.9    |
| realloc random (Mops/s) | 5.38       | **10.2**   | 8.33     | 5.26     | 7.45     |

### large

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 0.21   | **1.18** | 1.04     | 0.64     | 0.58     |
| large blocks (Mops/s)  | 0.10   | **0.10** | 0.10     | 0.00     | 0.01     |

### large, cold

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 0.21   | **1.15** | 0.99     | 0.53     | 0.53     |
| large blocks (Mops/s)  | 0.03   | **0.09** | 0.07     | 0.00     | 0.01     |

### memory

| Workload              | system    | bcmalloc | mimalloc | jemalloc | snmalloc  |
| --------------------- | --------- | -------- | -------- | -------- | --------- |
| burst retention (MiB) | **0.26**  | 10.8     | 260.3    | 269.0    | 261.3     |
| half freed (MiB)      | 183.3     | 157.5    | 156.0    | 157.0    | **153.3** |
| phase churn (MiB)     | **133.3** | 184.5    | 197.0    | 159.0    | 149.7     |

### latency

| Workload                     | system | bcmalloc | mimalloc | jemalloc  | snmalloc |
| ---------------------------- | ------ | -------- | -------- | --------- | -------- |
| latency churn 1 thread (ns)  | 19.4   | **6.88** | 11.6     | 18.2      | 7.66     |
| latency churn 8 threads (ns) | 20.2   | **7.03** | 32.8     | 18.6      | 8.75     |
| latency remote frees (ns)    | 563.9  | 132.8    | 465.9    | **112.5** | 361.9    |

### latency, latency (p50 / p99 / p999)

| Workload                     | system                | bcmalloc             | mimalloc             | jemalloc             | snmalloc             |
| ---------------------------- | --------------------- | -------------------- | -------------------- | -------------------- | -------------------- |
| latency churn 1 thread (ns)  | 11.6 / 19.4 / 68.0    | 3.91 / 6.88 / 9.23   | 7.34 / 11.6 / 17.7   | 4.45 / 18.2 / 23.1   | 5.00 / 7.66 / 15.6   |
| latency churn 8 threads (ns) | 12.0 / 20.2 / 63.3    | 3.92 / 7.03 / 10.2   | 12.8 / 32.8 / 54.1   | 5.16 / 18.6 / 23.8   | 5.16 / 8.75 / 20.3   |
| latency remote frees (ns)    | 141.1 / 563.9 / 795.5 | 21.1 / 132.8 / 200.3 | 99.2 / 465.9 / 662.0 | 41.9 / 112.5 / 170.9 | 26.7 / 361.9 / 478.8 |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.09   | 3.50     | 1.00     | 0.66     | 1.79     |
| speed, bulk            | 0.14   | 1.14     | 1.00     | 0.36     | 0.61     |
| speed, churn           | 0.83   | 2.28     | 1.00     | 1.68     | 1.73     |
| speed, sharing         | 0.33   | 2.07     | 1.00     | 2.19     | 1.13     |
| speed, false sharing   | 0.92   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.63   | 1.09     | 1.00     | 0.73     | 0.96     |
| speed, resizing        | 1.03   | 1.09     | 1.00     | 0.60     | 0.92     |
| speed, large           | 0.43   | 1.07     | 1.00     | 0.08     | 0.27     |
| speed, all workloads   | 0.64   | 1.82     | 1.00     | 0.89     | 1.18     |
| memory held afterwards | 10.77  | 2.94     | 1.00     | 1.06     | 1.10     |
| peak resident memory   | 2.02   | 1.76     | 1.00     | 1.77     | 1.49     |
| speed, mean of groups  | 0.56   | 1.49     | 1.00     | 0.64     | 0.91     |
| speed, cold            | 0.63   | 1.81     | 1.00     | 0.88     | 1.12     |
| latency, p99           | 0.93   | 3.03     | 1.00     | 1.67     | 1.94     |

</details>
<!-- /generated:dev:linux -->
