//! Helpers shared by the workloads.
//!
//! One operation in every workload is one allocation together with its free, so that throughput
//! figures can be compared across workloads. Workloads that resize or write report their own unit.
//! Latency workloads time batches of calls and report percentiles of the time of one call.

use std::{
    alloc::{Layout, alloc, dealloc},
    sync::Barrier,
    thread,
    time::{Duration, Instant},
};

/// How much work a workload does.
#[derive(Clone, Copy)]
pub(crate) struct Scale {
    quick: bool,
}

impl Scale {
    pub(crate) const fn new(quick: bool) -> Self {
        Self { quick }
    }

    /// `base` operations, or a quarter as many for a quick run.
    pub(crate) const fn ops(self, base: usize) -> usize {
        if self.quick { base / 4 } else { base }
    }
}

/// What a workload measured.
pub(crate) enum Outcome {
    /// Operations done and the time they took.
    Throughput { ops: u64, elapsed: Duration },
    /// A memory footprint in MiB.
    Footprint(f64),
    /// Percentiles of the time of one call, in nanoseconds.
    Latency { p50: f64, p99: f64, p999: f64 },
}

/// Runs `work` on this thread and times it. The closure returns how many operations it did.
pub(crate) fn timed(work: impl FnOnce() -> u64) -> Outcome {
    let start = Instant::now();
    let ops = work();
    Outcome::Throughput { ops, elapsed: start.elapsed() }
}

/// Runs `body` on `threads` threads that start together. Each call returns the operations it did.
/// The clock covers the time from the common start to the last thread finishing.
pub(crate) fn parallel(threads: usize, body: impl Fn(usize) -> u64 + Sync) -> Outcome {
    let barrier = Barrier::new(threads + 1);
    thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|id| {
                let (barrier, body) = (&barrier, &body);
                scope.spawn(move || {
                    barrier.wait();
                    body(id)
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        let ops = handles.into_iter().map(|handle| handle.join().unwrap()).sum();
        Outcome::Throughput { ops, elapsed: start.elapsed() }
    })
}

/// Runs `body` on `threads` threads that start together and collects what each returns, in thread
/// order. Unlike [`parallel`] it does no timing, for workloads that time themselves.
pub(crate) fn parallel_collect<T: Send>(
    threads: usize,
    body: impl Fn(usize) -> T + Sync,
) -> Vec<T> {
    let barrier = Barrier::new(threads);
    thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|id| {
                let (barrier, body) = (&barrier, &body);
                scope.spawn(move || {
                    barrier.wait();
                    body(id)
                })
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    })
}

/// How many calls one latency sample covers. A clock read costs about as much as a fast
/// allocation, so timing a batch keeps the clock out of the figure. The price is that a sample is
/// an average over its batch, which smooths out a single slow call.
pub(crate) const BATCH: usize = 32;

/// Times `work` and returns the elapsed nanoseconds. The clock on macOS ticks every 41 ns, which
/// is small next to a batch.
pub(crate) fn time_batch(work: impl FnOnce()) -> u32 {
    let start = Instant::now();
    work();
    start.elapsed().as_nanos().min(u128::from(u32::MAX)) as u32
}

/// Turns the batch times of a run into percentiles of the time of one call. Each sample is the
/// time of a batch that made `calls` calls.
pub(crate) fn latency(mut samples: Vec<u32>, calls: usize) -> Outcome {
    assert!(!samples.is_empty(), "no latency samples");
    samples.sort_unstable();
    let at = |quantile: f64| {
        let rank = (samples.len() as f64 * quantile).ceil() as usize;
        f64::from(samples[rank.clamp(1, samples.len()) - 1]) / calls as f64
    };
    Outcome::Latency { p50: at(0.5), p99: at(0.99), p999: at(0.999) }
}

/// A tiny xorshift generator, so that every allocator sees the same sequence of requests.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) const fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }

    /// A value in 0..n.
    pub(crate) const fn below(&mut self, n: usize) -> usize {
        (self.next() >> 8) % n
    }

    /// A value in lo..=hi.
    pub(crate) const fn between(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// Shuffles a slice.
    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// A block from the global allocator that is freed when dropped and can move between threads.
pub(crate) struct Block {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: a block is plain owned memory.
unsafe impl Send for Block {}

impl Block {
    /// Allocates `size` bytes and touches the first byte, as a program using the block would.
    pub(crate) fn new(size: usize) -> Self {
        let ptr = raw_alloc(size);
        // SAFETY: the allocation has at least one byte.
        unsafe { ptr.write(1) };
        Self { ptr, size }
    }

    /// Writes the first byte.
    pub(crate) fn touch(&self, value: u8) {
        // SAFETY: the allocation has at least one byte.
        unsafe { self.ptr.write_volatile(value) };
    }

    /// Writes one byte in every page, which makes the memory resident.
    pub(crate) fn touch_pages(&self) {
        for offset in (0..self.size).step_by(4096) {
            // SAFETY: the offset is inside the allocation.
            unsafe { self.ptr.add(offset).write_volatile(1) };
        }
    }

    /// Fills the whole block with a pattern that depends on `seed` and returns a hash of what it
    /// reads back, as a program would when it puts data in a new block and then uses it.
    pub(crate) fn fill_and_hash(&mut self, seed: u64) -> u64 {
        // SAFETY: the pointer is valid for `size` bytes and this block owns them. The slice is
        // written before it is read, and so no byte is read uninitialized.
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) };
        let mut word = seed | 1;
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
            word = word.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        }
        let mut hash = 0xCBF2_9CE4_8422_2325_u64;
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            hash = (hash ^ u64::from_le_bytes(word)).wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        raw_free(self.ptr, self.size);
    }
}

/// Allocates `size` bytes with the alignment of a word. A zero size is served as one byte.
pub(crate) fn raw_alloc(size: usize) -> *mut u8 {
    // SAFETY: the layout has a non-zero size.
    let ptr = unsafe { alloc(layout(size)) };
    assert!(!ptr.is_null(), "out of memory");
    ptr
}

/// Frees a block from [`raw_alloc`].
pub(crate) fn raw_free(ptr: *mut u8, size: usize) {
    // SAFETY: the caller got the pointer from raw_alloc with this size.
    unsafe { dealloc(ptr, layout(size)) };
}

pub(crate) fn layout(size: usize) -> Layout {
    Layout::from_size_align(size.max(1), 8).expect("valid layout")
}

/// A table slot that holds a block or nothing.
pub(crate) fn empty_slots(count: usize) -> Vec<Option<Block>> {
    (0..count).map(|_| None).collect()
}

/// The memory this process holds right now in MiB. On macOS this is the physical footprint, which
/// unlike the resident size does not count pages that were freed with a lazy advice.
pub(crate) fn resident_mib() -> f64 {
    resident_bytes() as f64 / f64::from(1 << 20)
}

#[cfg(target_os = "macos")]
fn rusage() -> libc::rusage_info_v4 {
    // SAFETY: proc_pid_rusage fills the struct we pass, and a zeroed one is a valid start.
    unsafe {
        let mut info = std::mem::zeroed::<libc::rusage_info_v4>();
        let buffer = std::ptr::from_mut(&mut info).cast();
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, buffer);
        info
    }
}

#[cfg(target_os = "macos")]
fn resident_bytes() -> u64 {
    rusage().ri_phys_footprint
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm.split_whitespace().nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages * page as u64
}

/// The most memory this process has held, in MiB.
#[cfg(target_os = "macos")]
pub(crate) fn peak_resident_mib() -> f64 {
    rusage().ri_lifetime_max_phys_footprint as f64 / f64::from(1 << 20)
}

/// The most memory this process has held, in MiB.
#[cfg(target_os = "linux")]
pub(crate) fn peak_resident_mib() -> f64 {
    // SAFETY: getrusage fills the struct we pass and has no other effects.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    usage.ru_maxrss as f64 * 1024.0 / f64::from(1 << 20)
}
