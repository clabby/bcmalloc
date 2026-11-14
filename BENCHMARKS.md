# Benchmarks

bcmalloc against the system allocator, mimalloc, jemalloc and snmalloc on three sets of programs. The development suite of 51 synthetic workloads is what the allocator is tuned against. The held-out Rust programs and the C programs of [mimalloc-bench](https://github.com/daanx/mimalloc-bench) are never tuned against, and show whether the gains carry over to code written for other purposes.

![For every suite and every other allocator, one dot per workload showing how much faster bcmalloc is](assets/benchmarks.svg)

Every number on this page, and the figure, comes from [benchmarks.json](benchmarks.json). The benchmark commands record their results there, and `just bench-render` renders this page's tables, the table in the README and the figure from it. CI fails when they are out of date.

## Method

- **Machines.** An Apple M4 Max, where the system allocator is libmalloc, and a 32-core AMD EPYC 9354P with Ubuntu 24.04 and glibc 2.39. mimalloc-bench ran on a second, idle machine of the same model. Both Linux machines have transparent huge pages set to `madvise`, and the [machines table](#machines) gives the exact systems.
- **Versions.** The Rust suites use the latest published crates, mimalloc 0.1.52 (mimalloc 3.3.2), tikv-jemallocator 0.7 (jemalloc 5.3.1) and snmalloc-rs 0.7.6. mimalloc-bench pins mimalloc 1.8.2, so it was rebuilt with mimalloc 3.6.1, the latest release, next to jemalloc 5.4.0 and snmalloc 0.7.6. Rust is 1.100.
- **Runs.** Every Rust workload runs ten times per allocator, in a fresh process each time, with the allocators taking turns in a rotating order. The tables show medians, and bold marks the best median and every median whose 95% bootstrap interval overlaps it. The cold rows are the first pass of each workload and the others the steady state. Each mimalloc-bench program ran three times per allocator, and its table shows the medians of wall time and peak resident memory.
- **Reproducing.** `just bench`, `just bench-apps` and `just mimalloc-bench` run the three sets. `just bench --save PATH` keeps the raw samples, `just bench --load PATH` prints their report again, and `just bench-compare OLD NEW` compares two of them.

## Summary

<!-- generated:overview -->
| Compared with mimalloc             | system | bcmalloc | jemalloc | snmalloc |
| ---------------------------------- | ------ | -------- | -------- | -------- |
| development suite, macOS, speed    | 0.52   | 2.09     | 0.87     | 1.22     |
| development suite, Linux, speed    | 0.63   | 1.73     | 0.88     | 1.14     |
| Rust programs, macOS, speed        | 0.91   | 1.02     | 0.93     | 1.01     |
| Rust programs, Linux, speed        | 0.82   | 1.09     | 1.03     | 1.04     |
| mimalloc-bench, Linux, speed       | -      | 1.00     | 0.82     | 0.94     |
| mimalloc-bench, Linux, peak memory | -      | 1.51     | 1.38     | 1.40     |
<!-- /generated:overview -->

Each figure is mimalloc's time or peak memory divided by the other allocator's, so above 1.00 means faster or smaller. It is the geometric mean over the suite's speed workloads, or over its programs for memory.

bcmalloc is the fastest allocator on both development suites, by a wide margin in most groups, though jemalloc is slightly ahead on the sharing group on Linux. It is the fastest on the Rust programs on both machines. On mimalloc-bench it ties mimalloc for time while holding two thirds of its peak memory, and it is faster than snmalloc and jemalloc. It is not the fastest on every program there. mimalloc is faster on sh6bench and sh8bench, which free thousands of blocks in a row, more than a heap's bins hold, and on alloc-test, glibc-simple, mstress and rocksdb by 3% to 7%. snmalloc is faster on redis, glibc-thread, cfrac and espresso by 1% to 6%, and on xmalloc-test, where jemalloc is faster still. The 0.01 s difference on mleak10 is the resolution of the timer.

## Machines

<!-- generated:machines -->
| Suite                    | Machine                                                           | Runs | Date       |
| ------------------------ | ----------------------------------------------------------------- | ---- | ---------- |
| development suite, macOS | Apple M4 Max, macOS 27.0.1                                        | 10   | 2026-11-14 |
| development suite, Linux | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 7.0.0 | 10   | 2026-11-14 |
| Rust programs, macOS     | Apple M4 Max, macOS 27.0.1                                        | 10   | 2026-11-14 |
| Rust programs, Linux     | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 7.0.0 | 10   | 2026-11-14 |
| mimalloc-bench, Linux    | AMD EPYC 9354P 32-Core Processor, Ubuntu 24.04.2 LTS, Linux 6.8.0 | 3    | 2026-11-14 |
<!-- /generated:machines -->

## mimalloc-bench

The geometric means, then the wall time in seconds and the peak resident memory in MiB of every program, as `time / memory`, with the best of each in bold. lua is left out, because its sources are not part of the suite as built.

<!-- generated:mimalloc-bench:linux -->
| Compared with mimalloc | bcmalloc | mimalloc | snmalloc | jemalloc |
| ---------------------- | -------- | -------- | -------- | -------- |
| speed                  | 1.00     | 1.00     | 0.94     | 0.82     |
| peak resident memory   | 1.51     | 1.00     | 1.40     | 1.38     |

| Program        | bcmalloc             | mimalloc           | snmalloc            | jemalloc           |
| -------------- | -------------------- | ------------------ | ------------------- | ------------------ |
| cfrac          | 3.250 / **3.2**      | 3.240 / 4.4        | **3.160** / 3.5     | 3.270 / 5.2        |
| espresso       | 3.470 / **3.8**      | 3.470 / 10.4       | **3.420** / 4.5     | 3.540 / 6.2        |
| barnes         | **2.170** / 63.5     | 2.210 / 67.5       | 2.260 / **58.2**    | 2.250 / 59.5       |
| redis          | 3.319 / 11.2         | 3.420 / 20.1       | **3.120** / **8.0** | 3.740 / 10.0       |
| larsonN        | **1.071** / 342.1    | 1.167 / 306.5      | 1.219 / 415.8       | 1.290 / **260.0**  |
| larsonN-sized  | **1.030** / 347.9    | 1.126 / 310.4      | 1.183 / 418.0       | 1.161 / **270.1**  |
| mstressN       | 0.670 / 598.2        | **0.630** / 1030.0 | 0.820 / 813.5       | 1.230 / **511.0**  |
| rptestN        | **0.199** / 222.8    | 0.237 / 236.0      | 0.215 / 183.0       | 0.511 / **161.0**  |
| gs             | **0.220** / 41.8     | 0.230 / 55.3       | 0.240 / **36.0**    | 0.240 / 36.8       |
| alloc-test1    | 2.640 / 14.2         | **2.500** / 16.2   | 2.590 / 13.8        | 2.580 / **12.8**   |
| alloc-testN    | 2.460 / **15.8**     | **2.300** / 60.3   | 2.310 / 16.2        | 2.330 / 16.2       |
| sh6benchN      | 0.060 / 280.1        | **0.050** / 226.4  | 0.080 / 275.0       | 0.090 / **224.3**  |
| sh8benchN      | 0.170 / 251.3        | **0.140** / 510.5  | 0.190 / 205.3       | 0.270 / **168.2**  |
| xmalloc-testN  | 0.191 / 103.0        | 0.216 / **66.3**   | 0.177 / 128.3       | **0.165** / 141.6  |
| cache-scratch1 | **1.050** / **4.0**  | **1.050** / 6.2    | **1.050** / 4.2     | **1.050** / 4.5    |
| cache-scratchN | **0.030** / 4.0      | **0.030** / 6.2    | **0.030** / **3.8** | **0.030** / 4.5    |
| glibc-simple   | 1.670 / **2.0**      | **1.590** / 4.2    | 1.620 / 2.5         | 1.900 / 4.8        |
| glibc-thread   | 0.203 / **10.8**     | 0.199 / 88.5       | **0.192** / 21.2    | 0.205 / 14.8       |
| rocksdb        | 4.270 / 102.7        | **4.130** / 141.9  | 4.350 / 107.3       | 4.350 / **102.5**  |
| leanN          | 9.100 / 260.4        | **9.080** / 335.9  | 9.110 / 242.5       | 9.280 / **218.9**  |
| linux          | **0.150** / **18.0** | 0.170 / 23.7       | **0.150** / 18.5    | 0.160 / 18.5       |
| malloc-large   | 1.420 / 579.6        | **1.400** / 588.1  | 3.560 / 650.5       | 14.420 / **429.6** |
| mleak10        | 0.150 / **2.0**      | 0.150 / 4.2        | **0.140** / 2.5     | 0.160 / 4.8        |
| mleak100       | **1.500** / **2.0**  | **1.500** / 4.2    | **1.500** / 2.5     | 1.610 / 4.8        |
| rbstress1      | **2.790** / 118.8    | 2.820 / 139.2      | 2.820 / **117.3**   | 2.800 / 119.0      |
| rbstressN      | 3.020 / **163.3**    | 3.010 / 192.7      | 3.010 / 209.0       | **2.990** / 201.3  |
| cache-thrash1  | **1.050** / **4.0**  | **1.050** / 6.2    | **1.050** / 4.2     | **1.050** / 4.5    |
| cache-thrashN  | **0.030** / **4.0**  | **0.030** / 6.2    | **0.030** / **4.0** | **0.030** / 4.5    |
<!-- /generated:mimalloc-bench:linux -->

## Rust programs

Each program is a crate written for its own purpose and driven with a fixed input. They are built on serde_json, regex, syn, a hyper server with its clients, and tokio channels.

### macOS

<!-- generated:apps:macos:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.91   | 1.02     | 1.00     | 0.93     | 1.01     |
| peak resident memory   | 1.22   | 0.96     | 1.00     | 1.13     | 1.17     |
| speed, cold            | 0.91   | 1.03     | 1.00     | 0.93     | 1.01     |
<!-- /generated:apps:macos:summary -->

<!-- generated:apps:macos -->
<details>
<summary>Every workload on macOS</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### apps

| Workload                | system | bcmalloc   | mimalloc  | jemalloc  | snmalloc   |
| ----------------------- | ------ | ---------- | --------- | --------- | ---------- |
| json (MB/s)             | 252.1  | 291.3      | **306.0** | 248.9     | 298.6      |
| regex (MB/s)            | 1452.0 | **1525.0** | 1515.0    | 1448.0    | **1526.0** |
| syn (MB/s)              | 16.3   | **20.4**   | 19.6      | 16.1      | 19.5       |
| http (kreq/s)           | 218.6  | **227.6**  | **225.6** | **225.7** | **225.3**  |
| tokio channels (Mmsg/s) | 4.04   | **4.59**   | 4.14      | 4.42      | 4.44       |

### apps, cold

| Workload                | system | bcmalloc   | mimalloc  | jemalloc  | snmalloc   |
| ----------------------- | ------ | ---------- | --------- | --------- | ---------- |
| json (MB/s)             | 254.8  | 291.3      | **303.3** | 247.6     | 298.2      |
| regex (MB/s)            | 1442.0 | **1522.0** | 1513.0    | 1445.0    | **1520.0** |
| syn (MB/s)              | 16.0   | **20.4**   | 19.5      | 15.9      | 19.3       |
| http (kreq/s)           | 217.3  | **228.1**  | **224.0** | **226.7** | **225.8**  |
| tokio channels (Mmsg/s) | 4.04   | **4.58**   | 4.13      | 4.46      | 4.48       |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.91   | 1.02     | 1.00     | 0.93     | 1.01     |
| peak resident memory   | 1.22   | 0.96     | 1.00     | 1.13     | 1.17     |
| speed, cold            | 0.91   | 1.03     | 1.00     | 0.93     | 1.01     |

</details>
<!-- /generated:apps:macos -->

### Linux

<!-- generated:apps:linux:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.82   | 1.09     | 1.00     | 1.03     | 1.04     |
| peak resident memory   | 2.23   | 1.75     | 1.00     | 1.70     | 1.68     |
| speed, cold            | 0.81   | 1.09     | 1.00     | 1.02     | 1.02     |
<!-- /generated:apps:linux:summary -->

<!-- generated:apps:linux -->
<details>
<summary>Every workload on Linux</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### apps

| Workload                | system | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------- | ------ | ---------- | -------- | -------- | -------- |
| json (MB/s)             | 131.2  | **193.4**  | 188.1    | 171.3    | 182.1    |
| regex (MB/s)            | 986.9  | **1049.0** | 1039.0   | 1034.0   | 1045.0   |
| syn (MB/s)              | 7.17   | **10.6**   | 10.0     | 9.12     | 9.67     |
| http (kreq/s)           | 101.2  | **122.6**  | 115.6    | 120.6    | 118.1    |
| tokio channels (Mmsg/s) | 1.22   | 1.88       | 1.41     | **1.93** | 1.75     |

### apps, cold

| Workload                | system | bcmalloc   | mimalloc | jemalloc  | snmalloc |
| ----------------------- | ------ | ---------- | -------- | --------- | -------- |
| json (MB/s)             | 127.8  | **192.6**  | 185.3    | 166.8     | 177.0    |
| regex (MB/s)            | 978.5  | **1048.0** | 1039.0   | 1025.0    | 1036.0   |
| syn (MB/s)              | 7.02   | **10.6**   | 9.98     | 8.88      | 9.43     |
| http (kreq/s)           | 101.1  | **122.2**  | 115.6    | **121.6** | 118.2    |
| tokio channels (Mmsg/s) | 1.23   | 1.88       | 1.42     | **1.93**  | 1.75     |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, all workloads   | 0.82   | 1.09     | 1.00     | 1.03     | 1.04     |
| peak resident memory   | 2.23   | 1.75     | 1.00     | 1.70     | 1.68     |
| speed, cold            | 0.81   | 1.09     | 1.00     | 1.02     | 1.02     |

</details>
<!-- /generated:apps:linux -->

## Development suite

Eight groups of synthetic workloads, from single-thread loops to producer and consumer rings, Larson, false sharing, standard collections, resizing, large blocks, latency percentiles and memory after a burst.

### macOS

<!-- generated:dev:macos:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.56   | 3.75     | 1.00     | 0.55     | 1.71     |
| speed, bulk            | 0.27   | 1.48     | 1.00     | 0.43     | 0.81     |
| speed, churn           | 0.30   | 2.55     | 1.00     | 1.47     | 1.60     |
| speed, sharing         | 0.55   | 3.06     | 1.00     | 2.10     | 1.57     |
| speed, false sharing   | 0.87   | 1.00     | 1.00     | 0.68     | 1.00     |
| speed, collections     | 0.71   | 1.29     | 1.00     | 0.67     | 1.19     |
| speed, resizing        | 1.00   | 1.03     | 1.00     | 0.95     | 0.86     |
| speed, large           | 0.98   | 1.08     | 1.00     | 0.15     | 0.12     |
| speed, all workloads   | 0.52   | 2.09     | 1.00     | 0.87     | 1.22     |
| memory held afterwards | 2.37   | 3.13     | 1.00     | 0.96     | 3.91     |
| peak resident memory   | 0.99   | 0.98     | 1.00     | 0.99     | 1.09     |
| speed, mean of groups  | 0.59   | 1.68     | 1.00     | 0.68     | 0.90     |
| speed, cold            | 0.52   | 2.05     | 1.00     | 0.87     | 1.23     |
| latency, p99           | 0.54   | 4.14     | 1.00     | 2.34     | 1.65     |
<!-- /generated:dev:macos:summary -->

<!-- generated:dev:macos -->
<details>
<summary>Every workload on macOS</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### pair

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| --------------------- | ------ | --------- | -------- | -------- | -------- |
| pair 16 B (Mops/s)    | 153.5  | **433.4** | 224.3    | 142.8    | 397.0    |
| pair 64 B (Mops/s)    | 137.6  | **405.0** | 219.9    | 142.2    | 386.5    |
| pair 256 B (Mops/s)   | 126.0  | **410.5** | 182.3    | 140.0    | 344.2    |
| pair 1 KiB (Mops/s)   | 54.2   | **409.8** | 150.8    | 133.1    | 282.1    |
| pair 4 KiB (Mops/s)   | 71.7   | **411.3** | 95.6     | 111.8    | 159.8    |
| pair 16 KiB (Mops/s)  | 61.2   | **308.7** | 87.5     | 54.7     | 158.1    |
| pair 64 KiB (Mops/s)  | 14.5   | **309.1** | 87.9     | 11.4     | 161.7    |
| pair 256 KiB (Mops/s) | 14.3   | **211.5** | 88.6     | 7.75     | 38.9     |
| pair 1 MiB (Mops/s)   | 14.2   | **212.6** | 4.06     | 7.77     | 19.4     |

### pair, cold

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| --------------------- | ------ | --------- | -------- | -------- | -------- |
| pair 16 B (Mops/s)    | 152.6  | **433.0** | 223.8    | 143.0    | 396.2    |
| pair 64 B (Mops/s)    | 137.4  | **405.2** | 219.6    | 142.2    | 386.1    |
| pair 256 B (Mops/s)   | 129.0  | **409.4** | 182.1    | 140.0    | 341.8    |
| pair 1 KiB (Mops/s)   | 54.2   | **409.8** | 149.7    | 133.6    | 280.8    |
| pair 4 KiB (Mops/s)   | 72.7   | **410.8** | 95.0     | 112.0    | 167.5    |
| pair 16 KiB (Mops/s)  | 61.0   | **309.6** | 86.9     | 54.9     | 157.1    |
| pair 64 KiB (Mops/s)  | 14.6   | **308.5** | 88.2     | 11.3     | 162.5    |
| pair 256 KiB (Mops/s) | 14.3   | **210.1** | 88.6     | 7.54     | 38.9     |
| pair 1 MiB (Mops/s)   | 14.3   | **209.7** | 4.05     | 7.71     | 19.6     |

### bulk

| Workload                     | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | -------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 86.5   | **306.4** | 172.4    | 66.2     | 211.8    |
| bulk 32 B shuffled (Mops/s)  | 39.9   | **191.4** | 169.7    | 60.3     | 160.8    |
| bulk 512 B shuffled (Mops/s) | 10.8   | **101.6** | 63.0     | 35.5     | 29.1     |

### bulk, cold

| Workload                     | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | -------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 82.1   | **242.3** | 152.9    | 62.7     | 188.1    |
| bulk 32 B shuffled (Mops/s)  | 39.8   | **173.8** | 150.6    | 58.0     | 147.2    |
| bulk 512 B shuffled (Mops/s) | 10.5   | **55.3**  | 43.8     | 25.5     | 25.2     |

### churn

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------------------- | ------ | ---------- | -------- | -------- | -------- |
| churn small (Mops/s)                | 48.4   | **231.8**  | 98.1     | 122.8    | 214.4    |
| churn medium (Mops/s)               | 25.9   | **229.2**  | 58.5     | 110.2    | 118.6    |
| churn mixed (Mops/s)                | 35.5   | **227.9**  | 83.2     | 113.0    | 127.8    |
| churn skewed (Mops/s)               | 48.9   | **185.3**  | 155.2    | 127.6    | 174.0    |
| churn mixed 4 threads (Mops/s)      | 38.1   | **885.3**  | 289.0    | 442.5    | 436.7    |
| churn mixed 8 threads (Mops/s)      | 66.0   | **1730.0** | 422.5    | 865.3    | 810.1    |
| churn mixed 16 threads (Mops/s)     | 93.2   | **1952.0** | 595.5    | 1071.0   | 1033.0   |
| churn skewed 8 threads (Mops/s)     | 170.0  | **1373.0** | 848.5    | 950.1    | 1239.0   |
| churn mixed oversubscribed (Mops/s) | 120.5  | **2015.0** | 750.4    | 1104.0   | 1073.0   |
| churn large set (Mops/s)            | 6.14   | **53.8**   | 10.0     | 27.5     | 22.3     |
| churn with work (Mops/s)            | 3.15   | **3.42**   | 3.38     | 3.36     | 3.40     |

### churn, cold

| Workload                            | system | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------------------- | ------ | ---------- | -------- | -------- | -------- |
| churn small (Mops/s)                | 48.1   | **230.7**  | 98.1     | 122.5    | 213.7    |
| churn medium (Mops/s)               | 25.9   | **228.0**  | 58.6     | 109.7    | 118.8    |
| churn mixed (Mops/s)                | 35.3   | **226.9**  | 82.9     | 113.2    | 127.4    |
| churn skewed (Mops/s)               | 48.1   | **184.2**  | 153.1    | 127.5    | 173.3    |
| churn mixed 4 threads (Mops/s)      | 37.3   | **876.9**  | 294.4    | 441.9    | 434.4    |
| churn mixed 8 threads (Mops/s)      | 64.9   | **1703.0** | 436.8    | 863.8    | 809.0    |
| churn mixed 16 threads (Mops/s)     | 90.3   | **1900.0** | 614.1    | 1048.0   | 1007.0   |
| churn skewed 8 threads (Mops/s)     | 165.5  | **1404.0** | 842.3    | 966.8    | 1254.0   |
| churn mixed oversubscribed (Mops/s) | 93.7   | **1938.0** | 786.0    | 1127.0   | 1043.0   |
| churn large set (Mops/s)            | 5.74   | **35.8**   | 8.91     | 20.5     | 20.0     |
| churn with work (Mops/s)            | 3.15   | **3.41**   | 3.38     | 3.35     | 3.40     |

### sharing

| Workload                        | system | bcmalloc   | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | ---------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 36.3   | **1630.0** | 671.1    | 882.0     | 730.3    |
| ring 4 threads (Mops/s)         | 111.1  | **526.5**  | 103.2    | 435.0     | 172.2    |
| ring 8 threads (Mops/s)         | 111.4  | **556.5**  | 82.2     | **559.4** | 174.0    |
| channel 1 pair (Mops/s)         | 28.9   | **76.2**   | 25.6     | 53.4      | 46.8     |
| channel 4 pairs (Mops/s)        | 45.2   | 123.1      | 56.9     | **131.3** | 114.3    |
| shared table 8 threads (Mops/s) | 22.2   | **94.6**   | 39.1     | 88.6      | 35.7     |
| thread lifecycle (Mops/s)       | 36.4   | **196.7**  | 102.3    | 44.1      | 190.0    |

### sharing, cold

| Workload                        | system | bcmalloc   | mimalloc | jemalloc  | snmalloc  |
| ------------------------------- | ------ | ---------- | -------- | --------- | --------- |
| larson 8 threads (Mops/s)       | 35.6   | **1543.0** | 663.3    | 867.1     | 727.2     |
| ring 4 threads (Mops/s)         | 100.0  | **530.2**  | 102.8    | 439.5     | 171.4     |
| ring 8 threads (Mops/s)         | 110.1  | **546.0**  | 78.7     | **543.9** | 175.5     |
| channel 1 pair (Mops/s)         | 29.0   | **75.8**   | 25.9     | 51.4      | 51.3      |
| channel 4 pairs (Mops/s)        | 45.0   | 121.8      | 56.1     | **135.2** | 112.9     |
| shared table 8 threads (Mops/s) | 16.9   | **94.2**   | 38.9     | **92.8**  | 35.1      |
| thread lifecycle (Mops/s)       | 36.2   | **184.7**  | 102.1    | 43.8      | **181.9** |

### false sharing

| Workload                         | system  | bcmalloc    | mimalloc | jemalloc | snmalloc    |
| -------------------------------- | ------- | ----------- | -------- | -------- | ----------- |
| cache scratch 8 threads (Mops/s) | 26420.0 | **31070.0** | 30940.0  | 14320.0  | **31000.0** |
| cache thrash 8 threads (Mops/s)  | 27210.0 | **31060.0** | 30910.0  | 30930.0  | 30990.0     |

### false sharing, cold

| Workload                         | system  | bcmalloc    | mimalloc | jemalloc | snmalloc |
| -------------------------------- | ------- | ----------- | -------- | -------- | -------- |
| cache scratch 8 threads (Mops/s) | 27750.0 | **31050.0** | 30940.0  | 13100.0  | 31020.0  |
| cache thrash 8 threads (Mops/s)  | 27910.0 | **31050.0** | 30950.0  | 30900.0  | 30990.0  |

### collections

| Workload                        | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ------------------------------- | ------ | --------- | -------- | -------- | -------- |
| hash map (Mops/s)               | 7.55   | **14.7**  | 11.7     | 13.0     | 13.7     |
| btree map (Mops/s)              | 5.12   | **6.71**  | 5.67     | 6.24     | 6.33     |
| strings (Mops/s)                | 7.56   | 8.79      | 8.77     | 7.61     | **8.89** |
| linked list (Mops/s)            | 92.3   | **223.0** | 148.3    | 66.9     | 187.0    |
| binary trees (Mops/s)           | 75.2   | **174.6** | 119.3    | 56.2     | 158.2    |
| tree rewrite (Mops/s)           | 79.7   | **195.1** | 128.0    | 57.7     | 170.9    |
| binary trees with work (Mops/s) | 54.3   | **91.2**  | 75.2     | 43.9     | 85.3     |

### collections, cold

| Workload                        | system | bcmalloc  | mimalloc | jemalloc | snmalloc |
| ------------------------------- | ------ | --------- | -------- | -------- | -------- |
| hash map (Mops/s)               | 7.40   | **13.9**  | 11.2     | 12.7     | 13.6     |
| btree map (Mops/s)              | 5.11   | **6.67**  | 5.56     | 6.13     | 6.24     |
| strings (Mops/s)                | 7.55   | 8.75      | 8.76     | 7.60     | **8.90** |
| linked list (Mops/s)            | 89.8   | **212.0** | 143.8    | 65.6     | 178.5    |
| binary trees (Mops/s)           | 74.3   | **171.8** | 117.7    | 55.9     | 153.9    |
| tree rewrite (Mops/s)           | 79.5   | **196.2** | 128.4    | 57.8     | 171.2    |
| binary trees with work (Mops/s) | 54.4   | **91.7**  | 75.1     | 43.8     | 88.6     |

### resizing

| Workload                | system     | bcmalloc  | mimalloc  | jemalloc  | snmalloc |
| ----------------------- | ---------- | --------- | --------- | --------- | -------- |
| vec growth (Mops/s)     | 857.4      | 675.7     | 630.7     | **947.2** | 826.3    |
| string growth (Mops/s)  | **3092.0** | 2984.0    | 3012.0    | 2622.0    | 1806.0   |
| many vectors (Mops/s)   | 288.3      | **305.1** | **302.5** | 296.5     | 299.7    |
| realloc random (Mops/s) | 9.10       | **12.9**  | 12.3      | 7.72      | 8.47     |

### resizing, cold

| Workload                | system     | bcmalloc  | mimalloc  | jemalloc  | snmalloc |
| ----------------------- | ---------- | --------- | --------- | --------- | -------- |
| vec growth (Mops/s)     | 871.8      | 665.8     | 630.5     | **950.4** | 807.9    |
| string growth (Mops/s)  | **3057.0** | 2964.0    | 2988.0    | 2365.0    | 1800.0   |
| many vectors (Mops/s)   | 284.2      | **303.1** | **300.6** | 292.1     | 297.2    |
| realloc random (Mops/s) | 9.11       | **12.9**  | 12.2      | 7.73      | 8.43     |

### large

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 1.69   | **2.01** | 1.49     | 1.22     | 0.58     |
| large blocks (Mops/s)  | 0.20   | 0.20     | **0.23** | 0.01     | 0.01     |

### large, cold

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 1.65   | **1.95** | 1.45     | 1.16     | 0.58     |
| large blocks (Mops/s)  | 0.16   | 0.16     | **0.19** | 0.01     | 0.01     |

### memory

| Workload              | system    | bcmalloc | mimalloc  | jemalloc | snmalloc  |
| --------------------- | --------- | -------- | --------- | -------- | --------- |
| burst retention (MiB) | 21.8      | 7.41     | 260.6     | 266.3    | **5.47**  |
| half freed (MiB)      | **153.2** | 154.0    | **153.2** | 155.9    | 153.6     |
| phase churn (MiB)     | 132.5     | 169.2    | 148.2     | 163.2    | **118.1** |

### latency

| Workload                     | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------------- | ------ | -------- | -------- | -------- | -------- |
| latency churn 1 thread (ns)  | 16.9   | **3.27** | 7.16     | 7.81     | 5.22     |
| latency churn 8 threads (ns) | 108.1  | **3.91** | 15.6     | 7.81     | 7.17     |
| latency remote frees (ns)    | 118.5  | **37.8** | 304.7    | 43.6     | 204.4    |

### latency, latency (p50 / p99 / p999)

| Workload                     | system               | bcmalloc            | mimalloc             | jemalloc           | snmalloc             |
| ---------------------------- | -------------------- | ------------------- | -------------------- | ------------------ | -------------------- |
| latency churn 1 thread (ns)  | 13.0 / 16.9 / 19.5   | 1.95 / 3.27 / 5.20  | 4.56 / 7.16 / 9.12   | 3.91 / 7.81 / 9.77 | 3.27 / 5.22 / 8.46   |
| latency churn 8 threads (ns) | 57.3 / 108.1 / 142.6 | 1.95 / 3.91 / 5.22  | 7.16 / 15.6 / 22.1   | 3.91 / 7.81 / 10.4 | 3.91 / 7.17 / 10.4   |
| latency remote frees (ns)    | 29.3 / 118.5 / 369.8 | 1.31 / 37.8 / 313.2 | 6.51 / 304.7 / 597.0 | 7.81 / 43.6 / 74.2 | 6.50 / 204.4 / 946.0 |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.56   | 3.75     | 1.00     | 0.55     | 1.71     |
| speed, bulk            | 0.27   | 1.48     | 1.00     | 0.43     | 0.81     |
| speed, churn           | 0.30   | 2.55     | 1.00     | 1.47     | 1.60     |
| speed, sharing         | 0.55   | 3.06     | 1.00     | 2.10     | 1.57     |
| speed, false sharing   | 0.87   | 1.00     | 1.00     | 0.68     | 1.00     |
| speed, collections     | 0.71   | 1.29     | 1.00     | 0.67     | 1.19     |
| speed, resizing        | 1.00   | 1.03     | 1.00     | 0.95     | 0.86     |
| speed, large           | 0.98   | 1.08     | 1.00     | 0.15     | 0.12     |
| speed, all workloads   | 0.52   | 2.09     | 1.00     | 0.87     | 1.22     |
| memory held afterwards | 2.37   | 3.13     | 1.00     | 0.96     | 3.91     |
| peak resident memory   | 0.99   | 0.98     | 1.00     | 0.99     | 1.09     |
| speed, mean of groups  | 0.59   | 1.68     | 1.00     | 0.68     | 0.90     |
| speed, cold            | 0.52   | 2.05     | 1.00     | 0.87     | 1.23     |
| latency, p99           | 0.54   | 4.14     | 1.00     | 2.34     | 1.65     |

</details>
<!-- /generated:dev:macos -->

### Linux

<!-- generated:dev:linux:summary -->
| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.06   | 2.90     | 1.00     | 0.65     | 1.72     |
| speed, bulk            | 0.13   | 1.15     | 1.00     | 0.33     | 0.56     |
| speed, churn           | 0.82   | 2.23     | 1.00     | 1.67     | 1.71     |
| speed, sharing         | 0.33   | 2.02     | 1.00     | 2.17     | 1.09     |
| speed, false sharing   | 0.90   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.61   | 1.07     | 1.00     | 0.72     | 0.94     |
| speed, resizing        | 1.02   | 1.09     | 1.00     | 0.60     | 0.90     |
| speed, large           | 0.44   | 1.10     | 1.00     | 0.08     | 0.23     |
| speed, all workloads   | 0.63   | 1.73     | 1.00     | 0.88     | 1.14     |
| memory held afterwards | 11.88  | 2.89     | 1.00     | 1.06     | 1.10     |
| peak resident memory   | 2.12   | 1.82     | 1.00     | 1.82     | 1.49     |
| speed, mean of groups  | 0.55   | 1.45     | 1.00     | 0.63     | 0.88     |
| speed, cold            | 0.62   | 1.73     | 1.00     | 0.86     | 1.08     |
| latency, p99           | 0.91   | 2.98     | 1.00     | 1.64     | 1.96     |
<!-- /generated:dev:linux:summary -->

<!-- generated:dev:linux -->
<details>
<summary>Every workload on Linux</summary>

Median of 10 runs. Bold marks the best median and every median whose 95% bootstrap interval overlaps it.

### pair

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc  |
| --------------------- | ------ | --------- | -------- | -------- | --------- |
| pair 16 B (Mops/s)    | 130.0  | 298.6     | 283.7    | 251.8    | **342.7** |
| pair 64 B (Mops/s)    | 130.6  | 289.3     | 241.1    | 249.6    | **335.8** |
| pair 256 B (Mops/s)   | 130.6  | 284.4     | 211.0    | 241.4    | **303.2** |
| pair 1 KiB (Mops/s)   | 130.0  | **291.6** | 140.6    | 215.1    | **279.2** |
| pair 4 KiB (Mops/s)   | 55.7   | **290.0** | 60.3     | 153.7    | 190.8     |
| pair 16 KiB (Mops/s)  | 54.9   | **185.0** | 55.3     | 54.3     | **186.8** |
| pair 64 KiB (Mops/s)  | 54.1   | **203.9** | 55.3     | 4.04     | 189.6     |
| pair 256 KiB (Mops/s) | 57.4   | **130.5** | 55.3     | 4.08     | 20.3      |
| pair 1 MiB (Mops/s)   | 57.3   | **130.7** | 4.48     | 4.06     | 9.29      |

### pair, cold

| Workload              | system | bcmalloc  | mimalloc | jemalloc | snmalloc  |
| --------------------- | ------ | --------- | -------- | -------- | --------- |
| pair 16 B (Mops/s)    | 130.5  | 300.8     | 283.4    | 251.6    | **342.6** |
| pair 64 B (Mops/s)    | 130.6  | 283.8     | 257.0    | 249.5    | **335.4** |
| pair 256 B (Mops/s)   | 130.6  | 293.2     | 211.6    | 241.2    | **303.1** |
| pair 1 KiB (Mops/s)   | 130.6  | **292.8** | 140.5    | 216.1    | **279.2** |
| pair 4 KiB (Mops/s)   | 55.7   | **289.9** | 60.3     | 153.1    | 190.8     |
| pair 16 KiB (Mops/s)  | 54.9   | **185.7** | 55.1     | 54.3     | **186.1** |
| pair 64 KiB (Mops/s)  | 54.1   | **206.2** | 55.3     | 4.06     | 188.4     |
| pair 256 KiB (Mops/s) | 57.3   | **134.0** | 55.2     | 4.07     | 20.3      |
| pair 1 MiB (Mops/s)   | 56.7   | **132.9** | 4.46     | 4.05     | 9.29      |

### bulk

| Workload                     | system | bcmalloc  | mimalloc  | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | --------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 22.2   | 172.0     | **190.7** | 52.7     | 125.1    |
| bulk 32 B shuffled (Mops/s)  | 9.07   | **105.1** | 87.2      | 44.2     | 62.7     |
| bulk 512 B shuffled (Mops/s) | 7.10   | **59.0**  | 42.4      | 11.3     | 16.2     |

### bulk, cold

| Workload                     | system | bcmalloc  | mimalloc  | jemalloc | snmalloc |
| ---------------------------- | ------ | --------- | --------- | -------- | -------- |
| bulk 32 B in order (Mops/s)  | 25.9   | **158.8** | **165.4** | 41.8     | 67.7     |
| bulk 32 B shuffled (Mops/s)  | 9.97   | **99.4**  | 80.6      | 37.0     | 43.7     |
| bulk 512 B shuffled (Mops/s) | 4.82   | **41.4**  | 32.1      | 6.38     | 7.62     |

### churn

| Workload                            | system   | bcmalloc   | mimalloc | jemalloc | snmalloc  |
| ----------------------------------- | -------- | ---------- | -------- | -------- | --------- |
| churn small (Mops/s)                | 61.4     | 119.4      | 69.7     | 108.5    | **125.1** |
| churn medium (Mops/s)               | 36.3     | **116.0**  | 41.6     | 82.0     | 80.8      |
| churn mixed (Mops/s)                | 38.7     | **118.0**  | 58.1     | 86.8     | 87.9      |
| churn skewed (Mops/s)               | 51.6     | **137.8**  | 126.2    | 119.8    | 120.8     |
| churn mixed 4 threads (Mops/s)      | 149.4    | **466.1**  | 126.0    | 340.6    | 342.8     |
| churn mixed 8 threads (Mops/s)      | 299.2    | **937.0**  | 245.2    | 676.3    | 662.2     |
| churn mixed 16 threads (Mops/s)     | 593.0    | **1713.0** | 466.4    | 1333.0   | 1304.0    |
| churn skewed 8 threads (Mops/s)     | 394.2    | **1078.0** | 860.1    | 936.2    | 946.0     |
| churn mixed oversubscribed (Mops/s) | 1094.0   | **3185.0** | 1216.0   | 2338.0   | 2423.0    |
| churn large set (Mops/s)            | 5.00     | **24.7**   | 6.44     | 10.0     | 10.8      |
| churn with work (Mops/s)            | **2.01** | **2.11**   | **2.16** | 1.95     | **2.08**  |

### churn, cold

| Workload                            | system | bcmalloc   | mimalloc | jemalloc   | snmalloc  |
| ----------------------------------- | ------ | ---------- | -------- | ---------- | --------- |
| churn small (Mops/s)                | 61.1   | 119.6      | 69.9     | 108.3      | **125.1** |
| churn medium (Mops/s)               | 36.1   | **115.1**  | 41.6     | 81.1       | 80.3      |
| churn mixed (Mops/s)                | 38.5   | **117.1**  | 57.9     | 85.9       | 86.9      |
| churn skewed (Mops/s)               | 50.7   | **136.2**  | 125.0    | 118.5      | 119.6     |
| churn mixed 4 threads (Mops/s)      | 147.4  | **457.8**  | 128.9    | 338.1      | 337.4     |
| churn mixed 8 threads (Mops/s)      | 290.8  | **913.7**  | 248.2    | 671.4      | 656.0     |
| churn mixed 16 threads (Mops/s)     | 558.2  | **1554.0** | 478.4    | **1316.0** | 1251.0    |
| churn skewed 8 threads (Mops/s)     | 390.5  | **1061.0** | 819.6    | 928.9      | 945.6     |
| churn mixed oversubscribed (Mops/s) | 992.4  | **3182.0** | 1416.0   | 2312.0     | 2406.0    |
| churn large set (Mops/s)            | 3.86   | **20.5**   | 6.09     | 6.57       | 6.29      |
| churn with work (Mops/s)            | 1.93   | **2.18**   | **2.06** | 1.96       | **2.04**  |

### sharing

| Workload                        | system | bcmalloc  | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | --------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 145.5  | **817.1** | 405.6    | 635.4     | 555.9    |
| ring 4 threads (Mops/s)         | 9.68   | 76.4      | 24.0     | **120.0** | 34.2     |
| ring 8 threads (Mops/s)         | 17.4   | 152.0     | 47.6     | **234.0** | 68.5     |
| channel 1 pair (Mops/s)         | 1.19   | 8.54      | 5.42     | **9.17**  | 4.94     |
| channel 4 pairs (Mops/s)        | 4.58   | 28.1      | 20.3     | **33.4**  | 19.0     |
| shared table 8 threads (Mops/s) | 4.31   | **82.0**  | 29.2     | 69.0      | 22.1     |
| thread lifecycle (Mops/s)       | 70.8   | **77.0**  | 69.8     | 62.5      | 69.0     |

### sharing, cold

| Workload                        | system | bcmalloc  | mimalloc | jemalloc  | snmalloc |
| ------------------------------- | ------ | --------- | -------- | --------- | -------- |
| larson 8 threads (Mops/s)       | 142.6  | **802.8** | 418.6    | 622.8     | 545.1    |
| ring 4 threads (Mops/s)         | 9.63   | 76.4      | 23.8     | **119.0** | 34.5     |
| ring 8 threads (Mops/s)         | 17.2   | 151.1     | 47.2     | **227.9** | 68.7     |
| channel 1 pair (Mops/s)         | 1.17   | **8.38**  | 5.47     | **8.89**  | 5.01     |
| channel 4 pairs (Mops/s)        | 4.55   | 28.1      | 20.8     | **32.5**  | 19.0     |
| shared table 8 threads (Mops/s) | 4.29   | **81.2**  | 28.9     | 68.3      | 21.9     |
| thread lifecycle (Mops/s)       | 70.3   | **76.7**  | 68.2     | 61.3      | 68.4     |

### false sharing

| Workload                         | system  | bcmalloc | mimalloc    | jemalloc | snmalloc    |
| -------------------------------- | ------- | -------- | ----------- | -------- | ----------- |
| cache scratch 8 threads (Mops/s) | 49250.0 | 59790.0  | **59990.0** | 59900.0  | **59970.0** |
| cache thrash 8 threads (Mops/s)  | 59770.0 | 59800.0  | **59990.0** | 59900.0  | **59990.0** |

### false sharing, cold

| Workload                         | system  | bcmalloc | mimalloc    | jemalloc | snmalloc    |
| -------------------------------- | ------- | -------- | ----------- | -------- | ----------- |
| cache scratch 8 threads (Mops/s) | 42410.0 | 59780.0  | **59960.0** | 59690.0  | **59960.0** |
| cache thrash 8 threads (Mops/s)  | 59760.0 | 59790.0  | **59970.0** | 59710.0  | **59960.0** |

### collections

| Workload                        | system | bcmalloc | mimalloc  | jemalloc | snmalloc  |
| ------------------------------- | ------ | -------- | --------- | -------- | --------- |
| hash map (Mops/s)               | 4.08   | **9.48** | 7.89      | 7.06     | 7.56      |
| btree map (Mops/s)              | 2.88   | **5.31** | 4.50      | 4.77     | 4.48      |
| strings (Mops/s)                | 6.04   | **7.25** | 7.18      | 6.01     | **7.21**  |
| linked list (Mops/s)            | 85.5   | 161.9    | **164.7** | 78.2     | 129.8     |
| binary trees (Mops/s)           | 62.4   | **96.2** | **96.0**  | 58.7     | 90.9      |
| tree rewrite (Mops/s)           | 37.5   | 104.8    | 101.1     | 55.9     | **107.2** |
| binary trees with work (Mops/s) | 45.2   | **51.5** | 47.7      | 36.1     | 41.9      |

### collections, cold

| Workload                        | system | bcmalloc  | mimalloc  | jemalloc | snmalloc  |
| ------------------------------- | ------ | --------- | --------- | -------- | --------- |
| hash map (Mops/s)               | 4.47   | **9.30**  | 7.49      | 6.38     | 6.05      |
| btree map (Mops/s)              | 3.62   | **5.30**  | 4.39      | 4.45     | 4.14      |
| strings (Mops/s)                | 5.72   | **7.24**  | 7.17      | 5.91     | 7.07      |
| linked list (Mops/s)            | 72.8   | **154.5** | **149.3** | 68.2     | 101.3     |
| binary trees (Mops/s)           | 57.1   | **92.8**  | **95.0**  | 56.1     | 85.6      |
| tree rewrite (Mops/s)           | 45.4   | 104.5     | 101.4     | 55.8     | **107.5** |
| binary trees with work (Mops/s) | 44.4   | **51.4**  | 47.6      | 35.9     | 41.2      |

### resizing

| Workload                | system     | bcmalloc | mimalloc | jemalloc | snmalloc |
| ----------------------- | ---------- | -------- | -------- | -------- | -------- |
| vec growth (Mops/s)     | **1820.0** | 1557.0   | 1348.0   | 280.5    | 978.0    |
| string growth (Mops/s)  | **1876.0** | 1794.0   | 1791.0   | 1816.0   | 1689.0   |
| many vectors (Mops/s)   | **332.2**  | 277.6    | 279.2    | 272.3    | 294.9    |
| realloc random (Mops/s) | 5.39       | **10.4** | 8.39     | 5.32     | 7.50     |

### resizing, cold

| Workload                | system     | bcmalloc   | mimalloc | jemalloc | snmalloc |
| ----------------------- | ---------- | ---------- | -------- | -------- | -------- |
| vec growth (Mops/s)     | 1440.0     | **1489.0** | 1310.0   | 273.7    | 793.4    |
| string growth (Mops/s)  | **1813.0** | 1771.0     | 1779.0   | 1763.0   | 1630.0   |
| many vectors (Mops/s)   | **326.0**  | 275.1      | 276.1    | 246.1    | 275.5    |
| realloc random (Mops/s) | 5.39       | **10.3**   | 8.39     | 5.30     | 7.45     |

### large

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 0.22   | **1.22** | 1.04     | 0.66     | 0.53     |
| large blocks (Mops/s)  | 0.10   | **0.11** | 0.10     | 0.00     | 0.01     |

### large, cold

| Workload               | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| medium blocks (Mops/s) | 0.22   | **1.20** | 1.00     | 0.55     | 0.49     |
| large blocks (Mops/s)  | 0.04   | **0.09** | **0.07** | 0.00     | 0.01     |

### memory

| Workload              | system    | bcmalloc | mimalloc | jemalloc | snmalloc  |
| --------------------- | --------- | -------- | -------- | -------- | --------- |
| burst retention (MiB) | **0.20**  | 10.7     | 260.1    | 269.1    | 261.3     |
| half freed (MiB)      | 183.2     | 157.4    | 156.0    | 157.0    | **153.4** |
| phase churn (MiB)     | **133.3** | 196.2    | 197.0    | 156.9    | 149.7     |

### latency

| Workload                     | system | bcmalloc | mimalloc | jemalloc  | snmalloc |
| ---------------------------- | ------ | -------- | -------- | --------- | -------- |
| latency churn 1 thread (ns)  | 19.2   | **6.72** | 11.4     | 18.2      | 7.50     |
| latency churn 8 threads (ns) | 19.7   | **6.88** | 31.5     | 18.6      | 8.45     |
| latency remote frees (ns)    | 566.7  | 134.8    | 457.3    | **109.9** | 347.0    |

### latency, latency (p50 / p99 / p999)

| Workload                     | system                | bcmalloc             | mimalloc              | jemalloc             | snmalloc             |
| ---------------------------- | --------------------- | -------------------- | --------------------- | -------------------- | -------------------- |
| latency churn 1 thread (ns)  | 11.6 / 19.2 / 64.4    | 3.91 / 6.72 / 9.53   | 7.34 / 11.4 / 17.2    | 4.53 / 18.2 / 23.1   | 5.07 / 7.50 / 15.6   |
| latency churn 8 threads (ns) | 12.0 / 19.7 / 59.4    | 4.06 / 6.88 / 10.0   | 12.6 / 31.5 / 51.6    | 5.17 / 18.6 / 23.6   | 5.00 / 8.45 / 20.0   |
| latency remote frees (ns)    | 185.9 / 566.7 / 752.2 | 20.2 / 134.8 / 198.0 | 101.3 / 457.3 / 650.0 | 41.2 / 109.9 / 161.3 | 26.6 / 347.0 / 459.9 |

### Summary

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.06   | 2.90     | 1.00     | 0.65     | 1.72     |
| speed, bulk            | 0.13   | 1.15     | 1.00     | 0.33     | 0.56     |
| speed, churn           | 0.82   | 2.23     | 1.00     | 1.67     | 1.71     |
| speed, sharing         | 0.33   | 2.02     | 1.00     | 2.17     | 1.09     |
| speed, false sharing   | 0.90   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.61   | 1.07     | 1.00     | 0.72     | 0.94     |
| speed, resizing        | 1.02   | 1.09     | 1.00     | 0.60     | 0.90     |
| speed, large           | 0.44   | 1.10     | 1.00     | 0.08     | 0.23     |
| speed, all workloads   | 0.63   | 1.73     | 1.00     | 0.88     | 1.14     |
| memory held afterwards | 11.88  | 2.89     | 1.00     | 1.06     | 1.10     |
| peak resident memory   | 2.12   | 1.82     | 1.00     | 1.82     | 1.49     |
| speed, mean of groups  | 0.55   | 1.45     | 1.00     | 0.63     | 0.88     |
| speed, cold            | 0.62   | 1.73     | 1.00     | 0.86     | 1.08     |
| latency, p99           | 0.91   | 2.98     | 1.00     | 1.64     | 1.96     |

</details>
<!-- /generated:dev:linux -->
