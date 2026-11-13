//! Allocation metrics, compiled in with the `metrics` feature.
//!
//! The counters follow the per-thread statistics of TCMalloc and jemalloc: each heap counts its
//! own events in words that only its thread writes, and a snapshot sums them over every heap slot.
//! A count is a relaxed load and a relaxed store of the heap's own word, which compiles to an
//! ordinary add on x86-64 and AArch64, with no locked instruction and no line that another core
//! writes. The allocation and free counters sit next to the bin they count, on a line the fast path
//! already touches. Everything else is counted on slow paths.
//!
//! Live bytes come from the counts and not from walking slabs, so a snapshot never reads state
//! that belongs to another thread. A snapshot taken while threads run sums counters that each only
//! move forward, so it is a consistent view of each counter, not an instant of the whole allocator.

use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    chunks::CHUNK_SHIFT,
    size_class::{CLASSES, Class},
};

/// A counter that one thread writes and any thread may read.
#[derive(Debug, Default)]
pub(crate) struct Counter(AtomicU64);

impl Counter {
    /// A counter at zero.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Adds `n`, wrapping. Only the owning thread may call this.
    #[inline(always)]
    pub(crate) fn add(&self, n: u64) {
        self.0.store(self.0.load(Ordering::Relaxed).wrapping_add(n), Ordering::Relaxed);
    }

    /// Subtracts `n`, wrapping. Only the owning thread may call this.
    #[inline(always)]
    pub(crate) fn sub(&self, n: u64) {
        self.add(n.wrapping_neg());
    }

    /// The current value.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A counter that any thread may add to, for the few paths that have no heap of their own.
#[derive(Debug, Default)]
pub(crate) struct SharedCounter(AtomicU64);

impl SharedCounter {
    /// A counter at zero.
    pub(crate) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Adds `n`.
    pub(crate) fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// The current value.
    pub(crate) fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The slow-path events a heap counts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Event {
    Refills,
    SlabsAdded,
    SlabsReleased,
    BlocksSent,
    MessagesPosted,
    MailReads,
    BlocksFiled,
    HugeAllocs,
    HugeFrees,
    HugeBytesAllocated,
    HugeBytesFreed,
    HugeCacheHits,
    BinsGrown,
    BinsShrunk,
}

/// How many kinds of [`Event`] there are.
const EVENTS: usize = Event::BinsShrunk as usize + 1;

/// The slow-path counters of one heap slot.
#[derive(Debug)]
pub(crate) struct HeapEvents([Counter; EVENTS]);

impl HeapEvents {
    /// Every counter at zero.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const fn new() -> Self {
        Self([const { Counter::new() }; EVENTS])
    }

    /// Counts `n` events of a kind. Only the heap's thread may call this.
    #[inline]
    pub(crate) fn add(&self, event: Event, n: u64) {
        self.0[event as usize].add(n);
    }

    /// The count of a kind.
    pub(crate) fn get(&self, event: Event) -> u64 {
        self.0[event as usize].get()
    }
}

/// The counters a region keeps for paths that run without a heap, and for the purge clock.
#[derive(Debug)]
pub(crate) struct RegionCounters {
    /// Small blocks freed by threads without a heap, by class.
    pub(crate) frees: [SharedCounter; CLASSES],
    pub(crate) huge_frees: SharedCounter,
    pub(crate) huge_bytes_freed: SharedCounter,
    pub(crate) purge_passes: SharedCounter,
    pub(crate) chunks_purged: SharedCounter,
    pub(crate) chunks_reused: SharedCounter,
}

impl RegionCounters {
    /// Every counter at zero.
    pub(crate) const fn new() -> Self {
        Self {
            frees: [const { SharedCounter::new() }; CLASSES],
            huge_frees: SharedCounter::new(),
            huge_bytes_freed: SharedCounter::new(),
            purge_passes: SharedCounter::new(),
            chunks_purged: SharedCounter::new(),
            chunks_reused: SharedCounter::new(),
        }
    }
}

/// What a snapshot reports for one size class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassMetrics {
    /// The size of every block of the class.
    pub block_size: usize,
    /// How many blocks have been handed out, including the allocator's own messages.
    pub allocs: u64,
    /// How many blocks have been freed.
    pub frees: u64,
    /// How many slabs of the class the heaps hold.
    pub slabs: u64,
    /// The bytes those slabs span.
    pub slab_bytes: u64,
}

impl ClassMetrics {
    /// Blocks handed out and not yet freed.
    pub const fn live_blocks(&self) -> u64 {
        self.allocs.saturating_sub(self.frees)
    }

    /// Bytes handed out and not yet freed.
    pub const fn live_bytes(&self) -> u64 {
        self.live_blocks() * self.block_size as u64
    }
}

/// Counts of the allocator's slow paths, summed over every heap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Events {
    /// Bins refilled from slabs.
    pub refills: u64,
    /// Slabs taken from the chunk allocator.
    pub slabs_added: u64,
    /// Empty slabs given back to the chunk allocator.
    pub slabs_released: u64,
    /// Blocks freed by a heap that does not own them and sent to their owner.
    pub blocks_sent: u64,
    /// Messages of several blocks posted to an owner's inbox.
    pub messages_posted: u64,
    /// Times a heap read its inbox.
    pub mail_reads: u64,
    /// Blocks a heap took in from its inbox.
    pub blocks_filed: u64,
    /// Huge blocks allocated.
    pub huge_allocs: u64,
    /// Huge blocks freed.
    pub huge_frees: u64,
    /// Huge requests served from a heap's cache of freed blocks.
    pub huge_cache_hits: u64,
    /// Bins that grew because they kept missing.
    pub bins_grown: u64,
    /// Bins that shrank because they stopped missing.
    pub bins_shrunk: u64,
    /// Passes of the purge clock.
    pub purge_passes: u64,
    /// Chunks the purge clock gave back to the operating system.
    pub chunks_purged: u64,
    /// Purged chunks that were used again.
    pub chunks_reused: u64,
}

/// A snapshot of the allocator's metrics, from [`Region::metrics`](crate::Region::metrics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metrics {
    /// The size of the managed memory.
    pub capacity: usize,
    /// Bytes of chunks that belong to a slab or a huge block, or that a heap keeps for reuse.
    pub chunk_bytes: usize,
    /// Heap slots that have ever been claimed.
    pub heaps_claimed: usize,
    /// Heap slots claimed now.
    pub heaps_in_use: usize,
    /// Bytes of huge blocks handed out and not yet freed.
    pub huge_live_bytes: u64,
    /// Every size class, in order.
    pub classes: [ClassMetrics; CLASSES],
    /// Slow-path counts.
    pub events: Events,
}

impl Metrics {
    /// Bytes of small blocks handed out and not yet freed, including the allocator's messages.
    pub fn small_live_bytes(&self) -> u64 {
        self.classes.iter().map(ClassMetrics::live_bytes).sum()
    }

    /// Bytes spanned by slabs.
    pub fn slab_bytes(&self) -> u64 {
        self.classes.iter().map(|class| class.slab_bytes).sum()
    }

    /// The share of slab bytes that holds live blocks, from 0 to 1. The rest is free blocks in
    /// slabs, blocks cached in bins and outboxes, and pages that no block has reached yet.
    pub fn slab_utilization(&self) -> f64 {
        let slabs = self.slab_bytes();
        if slabs == 0 { 1.0 } else { self.small_live_bytes() as f64 / slabs as f64 }
    }
}

/// Builds the per-class part of a snapshot from the allocations, frees and slabs of each class.
pub(crate) fn class_metrics(
    mut counts: impl FnMut(Class) -> (u64, u64, u64),
) -> [ClassMetrics; CLASSES] {
    core::array::from_fn(|index| {
        let class = Class::from_index(index);
        let (allocs, frees, slabs) = counts(class);
        ClassMetrics {
            block_size: class.block_size(),
            allocs,
            frees,
            slabs,
            slab_bytes: slabs * ((class.slab_chunks() as u64) << CHUNK_SHIFT),
        }
    })
}

/// A size in bytes with a binary unit, padded to the width the formatter asks for.
struct Bytes(u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.0 as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        let mut text = Text::default();
        if unit == 0 {
            write!(text, "{} B", self.0)?;
        } else {
            write!(text, "{value:.1} {}", UNITS[unit])?;
        }
        f.pad(text.as_str())
    }
}

/// A short string on the stack, so that formatting needs no allocation.
#[derive(Default)]
struct Text {
    bytes: [u8; 32],
    len: usize,
}

impl Text {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl Write for Text {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        if end > self.bytes.len() {
            return Err(fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use core::{alloc::Layout, ptr::NonNull};
    use std::{string::ToString, vec::Vec};

    use crate::{chunks::CHUNK_SIZE, size_class::Class, testing::TestRegion};

    #[test]
    fn snapshots_count_live_blocks_slabs_and_events() {
        let test = TestRegion::new(256, 2);
        let region = test.region();
        let (heap, other) = (region.heap().unwrap(), region.heap().unwrap());
        let small = Layout::from_size_align(100, 8).unwrap();
        let class = Class::for_size(100);
        let blocks: Vec<NonNull<u8>> = (0..1000).map(|_| heap.alloc(small).unwrap()).collect();
        let huge = Layout::from_size_align(48 * CHUNK_SIZE, 8).unwrap();
        let big = heap.alloc(huge).unwrap();

        let metrics = region.metrics();
        let counted = metrics.classes[class.index()];
        assert_eq!(counted.block_size, class.block_size());
        assert_eq!(counted.live_blocks(), 1000);
        assert!(counted.slabs >= 1 && counted.slab_bytes >= counted.live_bytes());
        assert_eq!(metrics.huge_live_bytes, huge.size().next_multiple_of(CHUNK_SIZE) as u64);
        assert_eq!(metrics.events.huge_allocs, 1);
        assert!(metrics.events.refills > 0 && metrics.events.slabs_added > 0);
        assert_eq!(metrics.heaps_in_use, 2);

        // Half go back on the owner, half from the other heap, which sends them home.
        let (mine, theirs) = blocks.split_at(500);
        // SAFETY: every block is live and freed once with its layout.
        unsafe {
            for &block in mine {
                heap.dealloc(block, small);
            }
            for &block in theirs {
                other.dealloc(block, small);
            }
            heap.dealloc(big, huge);
        }
        other.flush();
        heap.flush();
        other.flush();
        let metrics = region.metrics();
        let counted = metrics.classes[class.index()];
        assert_eq!(counted.live_blocks(), 0, "{counted:?}");
        assert_eq!(counted.allocs, 1000);
        assert_eq!(metrics.huge_live_bytes, 0);
        assert!(metrics.events.blocks_sent >= 500 && metrics.events.blocks_filed > 0);
        assert!(metrics.events.slabs_released > 0);
        assert!(metrics.small_live_bytes() == 0 && metrics.slab_utilization() <= 1.0);
        let report = metrics.to_string();
        assert!(report.starts_with("bcmalloc metrics"));
        assert!(report.contains(&std::format!("{:>8}", class.block_size())));
    }

    #[test]
    fn frees_by_threads_without_a_heap_are_counted_by_the_region() {
        let test = TestRegion::new(64, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = Layout::from_size_align(48, 8).unwrap();
        let block = heap.alloc(layout).unwrap();
        // SAFETY: the block is live and freed once.
        unsafe { region.dealloc(block, layout) };
        let counted = region.metrics().classes[Class::for_size(48).index()];
        assert_eq!((counted.allocs, counted.frees), (1, 1));
    }
}

impl fmt::Display for Metrics {
    /// A report in the manner of mimalloc's statistics: totals, then every class in use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let events = &self.events;
        writeln!(f, "bcmalloc metrics")?;
        writeln!(
            f,
            "  heaps: {} in use, {} claimed; chunks: {} of {}",
            self.heaps_in_use,
            self.heaps_claimed,
            Bytes(self.chunk_bytes as u64),
            Bytes(self.capacity as u64)
        )?;
        writeln!(
            f,
            "  small: {} live in {} of slabs ({:.0}% used); huge: {} live",
            Bytes(self.small_live_bytes()),
            Bytes(self.slab_bytes()),
            self.slab_utilization() * 100.0,
            Bytes(self.huge_live_bytes)
        )?;
        writeln!(
            f,
            "  refills {}, slabs +{} -{}, sent {} blocks in {} messages, mail read {} times for {} blocks",
            events.refills,
            events.slabs_added,
            events.slabs_released,
            events.blocks_sent,
            events.messages_posted,
            events.mail_reads,
            events.blocks_filed
        )?;
        writeln!(
            f,
            "  huge +{} -{} ({} cached), bins grown {} shrunk {}, purges {} gave back {} reused {}",
            events.huge_allocs,
            events.huge_frees,
            events.huge_cache_hits,
            events.bins_grown,
            events.bins_shrunk,
            events.purge_passes,
            Bytes(events.chunks_purged << CHUNK_SHIFT),
            Bytes(events.chunks_reused << CHUNK_SHIFT)
        )?;
        writeln!(
            f,
            "  {:>8} {:>14} {:>14} {:>11} {:>11} {:>5}",
            "size", "allocs", "frees", "live", "slabs", "used"
        )?;
        for class in self.classes.iter().filter(|class| class.allocs != 0 || class.slabs != 0) {
            let used = if class.slab_bytes == 0 {
                0.0
            } else {
                class.live_bytes() as f64 * 100.0 / class.slab_bytes as f64
            };
            writeln!(
                f,
                "  {:>8} {:>14} {:>14} {:>11} {:>11} {:>4.0}%",
                class.block_size,
                class.allocs,
                class.frees,
                Bytes(class.live_bytes()),
                Bytes(class.slab_bytes),
                used
            )?;
        }
        Ok(())
    }
}
