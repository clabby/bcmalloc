//! Configuration for a [`Region`](crate::Region) and for the global allocator.

use crate::chunks::CHUNK_SIZE;

/// How much of the reserved memory is backed by physical pages when the region is created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Commit {
    /// Pages are backed on first use. Start-up is instant and the footprint follows the workload.
    #[default]
    Lazy,
    /// Every page is backed before the region is returned, so the allocator never takes a page
    /// fault. The whole capacity counts as resident from the start.
    Populate,
    /// As `Populate`, and the pages are also pinned in RAM so they are never swapped out. This
    /// needs a high enough `RLIMIT_MEMLOCK`, and creating the region fails without it.
    Lock,
}

/// Region settings. Every method is `const`, so a configuration can sit in a `static`.
///
/// ```
/// use cwmalloc::{Commit, Config};
///
/// // A fixed 256 MiB heap that is fully resident and pinned, so allocation never page faults.
/// const CONFIG: Config = Config::new().capacity(256 << 20).max_heaps(64).commit(Commit::Lock);
/// assert_eq!(CONFIG.capacity_bytes(), 256 << 20);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    capacity: usize,
    max_heaps: usize,
    huge_pages: bool,
    commit: Commit,
}

impl Config {
    /// The defaults are 64 GiB of address space, 4096 heaps and lazy commit.
    pub const fn new() -> Self {
        Self { capacity: 64 << 30, max_heaps: 4096, huge_pages: false, commit: Commit::Lazy }
    }

    /// Sets the size of the managed region in bytes, rounded down to a multiple of 64 KiB.
    ///
    /// This is a hard limit and allocations fail once it is used up. It is only address space until
    /// memory is touched, so a generous value costs little.
    #[must_use]
    pub const fn capacity(mut self, bytes: usize) -> Self {
        self.capacity = bytes / CHUNK_SIZE * CHUNK_SIZE;
        self
    }

    /// Sets how many threads can allocate at once.
    ///
    /// Each running thread holds one heap, and a thread that finds none free fails its allocations.
    /// This bounds concurrency the way TigerBeetle bounds connections, so choose it from the design
    /// of the program instead of guessing.
    #[must_use]
    pub const fn max_heaps(mut self, heaps: usize) -> Self {
        self.max_heaps = heaps;
        self
    }

    /// Asks Linux to back the region with transparent huge pages. Other platforms ignore it.
    #[must_use]
    pub const fn huge_pages(mut self, enabled: bool) -> Self {
        self.huge_pages = enabled;
        self
    }

    /// Sets how eagerly the region is backed by physical memory.
    #[must_use]
    pub const fn commit(mut self, commit: Commit) -> Self {
        self.commit = commit;
        self
    }

    /// The configured region size in bytes.
    pub const fn capacity_bytes(&self) -> usize {
        self.capacity
    }

    /// The configured number of heaps.
    pub const fn heap_count(&self) -> usize {
        self.max_heaps
    }

    pub(crate) const fn wants_huge_pages(&self) -> bool {
        self.huge_pages
    }

    pub(crate) const fn commit_mode(&self) -> Commit {
        self.commit
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}
