//! The workloads shared by every allocator benchmark.
//!
//! A program has exactly one global allocator, so each allocator gets its own benchmark binary
//! that installs it and calls [`run`]. Results go to stdout as tab-separated rows so that the
//! `report` benchmark can line the allocators up side by side. Progress goes to stderr.
//!
//! Every figure is the best of several runs, which filters out interference from the rest of the
//! machine. Throughput is in millions of operations per second, so higher is better, except for
//! peak memory.

use std::{
    hint::black_box,
    mem::MaybeUninit,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Request sizes for the churn workloads, weighted towards small blocks like most programs.
const SIZES: [usize; 12] = [16, 24, 32, 48, 64, 64, 96, 128, 256, 512, 1024, 4096];

/// A block that is never read, so that the benchmark measures the allocator and not a memset.
type Block = Box<[MaybeUninit<u8>]>;

/// A tiny xorshift generator so that every allocator sees the same sequence of requests.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }
}

fn block(size: usize) -> Block {
    let mut block = Box::<[u8]>::new_uninit_slice(size);
    // Touch the first byte so the memory is really used, as a program would.
    block[0].write(1);
    block
}

/// Runs every workload against the allocator that this binary installed.
pub(crate) fn run(allocator: &str) {
    let quick = std::env::args().any(|arg| arg == "--quick");
    let runs = if quick { 2 } else { 5 };
    let scale = if quick { 1 } else { 4 };
    let cores = thread::available_parallelism().map_or(4, usize::from);
    let report = |workload: &str, value: f64, unit: &str| {
        eprintln!("{allocator:>10}  {workload:<28} {value:>10.2} {unit}");
        println!("{allocator}\t{workload}\t{value:.3}\t{unit}");
    };

    let ops = 1_000_000 * scale;
    report("single-thread churn", best(runs, || churn(1, ops)), "Mops/s");
    for threads in [4, 8, 16].into_iter().filter(|&t| t <= cores) {
        let name = format!("{threads}-thread churn");
        report(&name, best(runs, || churn(threads, ops)), "Mops/s");
    }
    for pairs in [1, 4].into_iter().filter(|&p| p * 2 <= cores) {
        let name = format!("{pairs}-pair cross-thread free");
        report(&name, best(runs, || cross_thread(pairs, 250_000 * scale)), "Mops/s");
    }
    report("vec growth", best(runs, || vec_growth(5 * scale)), "Mops/s");
    report("binary trees", best(runs, || binary_trees(16, 8 * scale)), "Mops/s");
    report("large blocks", best(runs, || large_blocks(200 * scale)), "Mops/s");
    report("peak memory", peak_memory_mib(), "MiB");
}

/// The best throughput over `runs` runs of a workload that returns how many operations it did.
fn best(runs: usize, mut workload: impl FnMut() -> (u64, Duration)) -> f64 {
    let _ = workload();
    (0..runs)
        .map(|_| {
            let (operations, elapsed) = workload();
            operations as f64 / elapsed.as_secs_f64() / 1e6
        })
        .fold(0.0, f64::max)
}

/// Each thread keeps 4096 live blocks and keeps replacing random ones, so blocks live for a while
/// and are freed out of order.
fn churn(threads: usize, ops_per_thread: usize) -> (u64, Duration) {
    let start = Instant::now();
    thread::scope(|scope| {
        for id in 0..threads {
            scope.spawn(move || {
                let mut rng = Rng(0x9E37_79B9 + id as u64 * 7919);
                let mut live: Vec<Option<Block>> = (0..4096).map(|_| None).collect();
                for _ in 0..ops_per_thread {
                    let slot = rng.next() % live.len();
                    live[slot] = Some(block(SIZES[rng.next() % SIZES.len()]));
                }
                black_box(live);
            });
        }
    });
    ((threads * ops_per_thread) as u64, start.elapsed())
}

/// Producers allocate blocks and send them to consumers on other threads, which free them. This is
/// the pattern that makes allocators with thread-local heaps either slow or hungry.
fn cross_thread(pairs: usize, messages: usize) -> (u64, Duration) {
    let start = Instant::now();
    thread::scope(|scope| {
        for pair in 0..pairs {
            let (sender, receiver) = mpsc::sync_channel::<Block>(512);
            scope.spawn(move || {
                let mut rng = Rng(0xBEEF + pair as u64);
                for _ in 0..messages {
                    sender.send(block(32 + rng.next() % 480)).unwrap();
                }
            });
            scope.spawn(move || {
                for received in receiver {
                    black_box(&received);
                }
            });
        }
    });
    ((pairs * messages) as u64, start.elapsed())
}

/// Grows vectors one push at a time, which is a long chain of reallocations.
fn vec_growth(repetitions: usize) -> (u64, Duration) {
    const ELEMENTS: u64 = 1_000_000;
    let start = Instant::now();
    for _ in 0..repetitions {
        let mut numbers = Vec::new();
        for i in 0..ELEMENTS {
            numbers.push(i);
        }
        black_box(&numbers);
    }
    (repetitions as u64 * ELEMENTS, start.elapsed())
}

struct Tree(Option<Box<(Self, Self)>>);

fn build(depth: u32) -> Tree {
    if depth == 0 { Tree(None) } else { Tree(Some(Box::new((build(depth - 1), build(depth - 1))))) }
}

fn count(tree: &Tree) -> u64 {
    tree.0.as_ref().map_or(0, |pair| 1 + count(&pair.0) + count(&pair.1))
}

/// The classic allocation benchmark. It builds and drops full binary trees, which is a flood of
/// tiny blocks that all die together.
fn binary_trees(depth: u32, repetitions: usize) -> (u64, Duration) {
    let start = Instant::now();
    let long_lived = build(depth);
    let mut nodes = 0;
    for _ in 0..repetitions {
        nodes += count(&build(depth));
    }
    black_box(&long_lived);
    (nodes, start.elapsed())
}

/// Allocates, touches and frees blocks of a few hundred kilobytes up to megabytes.
fn large_blocks(repetitions: usize) -> (u64, Duration) {
    const SIZES: [usize; 4] = [256 << 10, 1 << 20, 4 << 20, 16 << 20];
    let start = Instant::now();
    for i in 0..repetitions {
        let mut block = Box::<[u8]>::new_uninit_slice(SIZES[i % SIZES.len()]);
        for offset in (0..block.len()).step_by(4096) {
            block[offset].write(1);
        }
        black_box(&block);
    }
    (repetitions as u64, start.elapsed())
}

/// The peak resident set size of the whole process so far, in MiB.
fn peak_memory_mib() -> f64 {
    // SAFETY: getrusage fills the struct we pass and has no other effects.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    // macOS reports bytes and Linux reports kilobytes.
    let unit = if cfg!(target_os = "macos") { 1.0 } else { 1024.0 };
    usage.ru_maxrss as f64 * unit / (1 << 20) as f64
}
