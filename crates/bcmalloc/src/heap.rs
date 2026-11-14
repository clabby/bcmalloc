//! Per-thread heaps, which hold the allocation fast path and everything behind it.
//!
//! A [`Heap`] is one thread's private view of the region. It owns a set of slabs grouped by size
//! class, and it serves most requests from a bin, which is a small stack of free block pointers
//! kept per class. Allocating pops the bin and freeing pushes onto it, with no atomic operations,
//! so a block that was just freed is the next one handed out while it is still in the cache.
//!
//! ```text
//!   size class   bin (a stack of pointers)        active slabs (have room)       full slabs
//!   +-------+    +-----+-----+-----+-----+        +------+    +------+           +------+
//!   | 16 B  |--> | ptr | ptr | ptr |     |        | slab |--->| slab |           | slab |
//!   +-------+    +-----+-----+-----+-----+        +------+    +------+           +------+
//!   | 32 B  |     ^ pop and push here               ^ refills take blocks here
//!   +-------+
//!   | ...   |
//!   +-------+
//! ```
//!
//! The bins are the thread caches of jemalloc and the per-CPU arrays of TCMalloc. Keeping
//! pointers in an array, instead of a list threaded through the free blocks, means that handing
//! out a block never reads the memory of the block before it, and that a free never writes into a
//! block whose cache line another core holds.
//!
//! # Allocating
//!
//! 1. Pop the bin of the class. This is the whole fast path.
//! 2. If it is empty, read the inbox, where other heaps send blocks of this heap's slabs.
//! 3. Otherwise refill the bin to half its capacity from the front active slab, taking returned
//!    blocks first and then fresh ones from the slab's frontier. A slab that runs out moves to the
//!    full list, and a class with no active slab takes a fresh one from the chunk allocator.
//!
//! # Freeing
//!
//! The size class comes from the layout the caller passes, so no header is read. Whether the
//! block's slab belongs to this heap comes from the chunk map, one load from a dense array.
//!
//! ```text
//!                     +- own slab, or a small block that owns its cache lines -> push on the bin
//!   free(ptr, layout) |
//!                     +- another heap's slab -> append to the outbox for that heap
//!                                                  |
//!                                                  +-> a full outbox becomes a message in the
//!                                                      owner's inbox, one compare-and-swap
//! ```
//!
//! A heap may keep a block of another heap's slab in its own bin and hand it out again, as
//! jemalloc and TCMalloc do, but only for classes of at most 256 bytes whose blocks never share a
//! cache line with other blocks, and for messages. Otherwise the reusing thread would write next to
//! a block that the owner still uses, which is the false sharing that Hoard's rule of returning
//! blocks to their owner prevents, or pull across many lines that the owner still holds. Blocks of
//! the other classes always go home.
//!
//! A bin that fills up moves its oldest blocks into a spilled batch, an array of pointers that the
//! next refill of the class copies back, so that a burst of frees bigger than the bin costs a copy
//! of pointers each way. Batches that no refill wants go back to their slabs at the heap's next
//! maintenance step, and when the heap has spilled as much as it may, a full bin gives its oldest
//! half back at once: blocks of this heap's slabs go to the slab, and the others to their owners.
//!
//! # Messages
//!
//! Blocks travel home in messages, which is how snmalloc returns remote frees. A message here is
//! a 512-byte block of the sender's own that holds up to 62 pointers, so the sender never writes
//! into the blocks it sends and the owner never walks them to find the next one. A single block is
//! sent as itself, with a tag in its link. Each heap keeps a few outboxes, picked by the id of the
//! destination heap, and posts one when it fills, holds 64 KiB of blocks, or must make room for
//! another destination.
//!
//! The inbox of a heap is a stack that any thread pushes onto with one compare-and-swap and that
//! only the owner empties, with one swap. Nothing is ever popped by anyone else, so the stack
//! cannot suffer the ABA problem.
//!
//! # Heap lifetime
//!
//! A heap is a claimable object and not a property of a thread. When a thread exits, its heap
//! empties its bins and outboxes and is released with all its slabs intact, and the next thread to
//! start picks it up. Messages that arrive in between wait in its inbox.

use crate::{
    allocator::{Backend, allocator},
    cache::CacheLine,
    chunks::CHUNK_SHIFT,
    region::{Entry, HUGE_PAGE_CHUNKS, Region},
    size_class::{CLASSES, Class, MAX_SMALL, Request, STEP, TABLE_MAX, TABLE_STEPS, classify},
    slab::{Block, List, Owned, Returns, Slab},
    sync::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};
use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::Cell,
    fmt,
    marker::PhantomData,
    ptr,
    ptr::NonNull,
};

/// The tag on an inbox link that marks a node as a single block and not a message.
const SINGLE: usize = 0b1;

/// The size of a message, which is the block size of one class.
const MESSAGE_BYTES: usize = 512;

/// How many block pointers fit in a message after its link and its count.
const MESSAGE_BLOCKS: usize = MESSAGE_BYTES / size_of::<usize>() - 2;

/// The class that messages are allocated from.
const MESSAGE_CLASS: Class = Class::for_size(MESSAGE_BYTES);

/// How many outboxes a heap keeps. A remote free goes to the one picked by the destination heap's
/// id, so this many destinations can each fill a message without displacing each other.
const OUTBOXES: usize = 32;

/// The size of a spilled batch, which is the block size of one class.
const BATCH_BYTES: usize = 4096;

/// How many block pointers fit in a batch after its link and its count.
const BATCH_BLOCKS: usize = BATCH_BYTES / size_of::<usize>() - 2;

/// The class that batches are allocated from. Its own bin never spills, so that spilling never
/// needs a batch to make room for a batch.
const BATCH_CLASS: Class = Class::for_size(BATCH_BYTES);

/// How many bytes of blocks a heap may keep in spilled batches. Spilled blocks are blocks that the
/// program used a moment ago, so they add nothing to the peak, but they keep their slabs from going
/// back until a refill takes them or a maintenance step returns them. A budget of 1 MiB gave
/// sh8bench all the speed of 8 MiB, and many threads that each spill hold correspondingly less.
const SPILL_BUDGET: usize = 1 << 20;

/// An outbox is posted once it holds this many bytes of blocks, so that big blocks are not held
/// back for long. A single block this large is sent on its own at once.
const OUTBOX_BYTES: usize = 64 << 10;

/// How many refills pass between maintenance steps, which post the outboxes so that no block waits
/// in one for long, and shrink bins that have not missed since the last step.
const MAINTENANCE_INTERVAL: u32 = 64;

/// How many bytes of bin capacity a heap may add to the initial capacities of its bins.
const GROWTH_BUDGET: usize = 2 << 20;

/// How many freed huge blocks a heap keeps for reuse.
const HUGE_CACHE: usize = 4;

/// The largest huge block a heap keeps, and the most it keeps in all.
const HUGE_CACHE_BLOCK: usize = 8 << 20;
const HUGE_CACHE_BYTES: usize = 16 << 20;

/// Counts a slow-path event of the heap when metrics are compiled in.
macro_rules! count {
    ($heap:expr, $event:ident, $n:expr) => {
        #[cfg(feature = "metrics")]
        $heap.private.events.add(crate::metrics::Event::$event, $n as u64);
    };
}

/// The smallest block for which a zeroed request asks whether the memory is fresh. Clearing a
/// smaller block costs less than the slab lookup that would find out.
const ZERO_CHECK_MIN: usize = 1024;

/// Whether a heap may keep and reuse blocks of `class` that another heap owns. See
/// [`Class::reusable_anywhere`]. Messages qualify too, whatever their size: the heap that reads
/// one has its lines in its cache already, and reuses it for messages of its own.
const fn reusable_anywhere(class: Class) -> bool {
    class.reusable_anywhere() || class.index() == MESSAGE_CLASS.index()
}

/// How many blocks a bin that overflows gives back at a time, from a buffer on the stack.
const FLUSH_BATCH: usize = 512;

/// The size of a bin, which [`Entry`] uses to keep the offset of a class's bin.
pub(crate) const BIN_SIZE: usize = size_of::<Bin>();

/// For every size that the class table covers, in steps of [`STEP`], the byte offset of the
/// bin of its class in a heap's array of bins. The allocation fast path finds its bin with one load
/// where looking the class up and then the bin would take a load and a multiplication.
const BIN_OFFSETS: [u16; TABLE_STEPS] = {
    let mut offsets = [0; TABLE_STEPS];
    let mut step = 0;
    while step < offsets.len() {
        offsets[step] = (Class::from_table(step * STEP).index() * size_of::<Bin>()) as u16;
        step += 1;
    }
    offsets
};

/// How many chunks of filled slabs a heap holds before its chunks come from the zone of
/// transparent huge pages, 1 MiB. See [`Region::part`]. Below it are the threads that use a
/// little memory each, whose huge pages would be mostly empty.
const HUGE_ZONE_CHUNKS: usize = 16;

/// The last miss of a bin was a refill of an empty bin.
const UNDERFLOW: u8 = 0b01;

/// The last miss of a bin was a free into a full bin.
const OVERFLOW: u8 = 0b10;

/// The bin has missed since the last maintenance step.
const MISSED: u8 = 0b100;

/// The bin ran dry in this maintenance interval or the one before, so the heap allocates from the
/// class and a spilled batch has a refill to wait for. A heap that only frees a class never spills
/// it, and gives the blocks back at once.
const REFILLED: u8 = 0b1000;

/// Blocks on their way to the heap that owns their slabs.
///
/// A message is an ordinary block of [`MESSAGE_CLASS`] from the sender's heap. Its first word is
/// the inbox link, as for a single block, so both kinds of node share the link's place.
#[repr(C)]
struct Message {
    next: *mut Block,
    count: u32,
    /// The class of every block in the message, or [`MIXED`]. The owner files the blocks of a
    /// message of one class into its bin with one copy, without looking each block up.
    class: u32,
    blocks: [*mut Block; MESSAGE_BLOCKS],
}

/// The class of a message whose blocks are of more than one class.
const MIXED: u32 = u32::MAX;

const _: () = assert!(size_of::<Message>() == MESSAGE_BYTES);
const _: () = assert!(MESSAGE_CLASS.block_size() == MESSAGE_BYTES);

/// Blocks that overflowed a bin, kept for the next refill of their class.
///
/// A batch is an ordinary block of [`BATCH_CLASS`] from the heap's own slabs, and the batches of a
/// class form a stack, newest first, through `next`. `blocks` holds the blocks in the order they
/// sat in the bin, newest first.
#[repr(C)]
struct Batch {
    next: *mut Self,
    count: usize,
    blocks: [*mut Block; BATCH_BLOCKS],
}

const _: () = assert!(size_of::<Batch>() == BATCH_BYTES);
const _: () = assert!(BATCH_CLASS.block_size() == BATCH_BYTES);

/// A stack of free block pointers for one class. See the [module documentation](self).
///
/// The stack lives in the heap's slot array and grows down. It is empty when `top` is at the end of
/// its range and full when it is at the start. Both ends fit in 16 bits of address, because a bin
/// is at most 16 KiB long, so the checks compare the low bits of `top` only and the bin fits in one
/// load of 16 bytes.
struct Bin {
    top: Cell<*mut *mut Block>,
    empty: Cell<u16>,
    full: Cell<u16>,
    /// How many blocks the bin may hold now, between the class's initial and largest capacity.
    capacity: Cell<u16>,
    /// The kind of the last miss and whether the bin missed since the last maintenance step.
    misses: Cell<u8>,
    /// Whether this bin may hold blocks of other heaps' slabs.
    anywhere: Cell<bool>,
    /// Blocks of the class this heap has handed out and freed, on the bin's line.
    #[cfg(feature = "metrics")]
    allocs: crate::metrics::Counter,
    #[cfg(feature = "metrics")]
    frees: crate::metrics::Counter,
}

/// Blocks of other heaps' slabs that this heap has freed and not posted yet, all for one heap.
struct Outbox {
    dest: Cell<u32>,
    /// The message being filled, or null. It stays after a post that sent its one block alone.
    message: Cell<*mut Message>,
    bytes: Cell<u32>,
}

/// A huge block that this heap freed and keeps, still claimed in the chunk bitmap, for the next
/// request of the same size. An empty entry has no chunks.
#[derive(Clone, Copy)]
struct Kept {
    first: usize,
    chunks: usize,
    /// Whether it was reused since the last maintenance step.
    reused: bool,
}

impl Kept {
    const NONE: Self = Self { first: 0, chunks: 0, reused: false };
}

/// The slab lists of a size class. The active list is tracked from both ends.
struct Lists {
    active: Cell<*const Slab>,
    active_last: Cell<*const Slab>,
    full: Cell<*const Slab>,
    /// How many of the class's slabs that the heap holds have been filled once.
    filled: Cell<u32>,
    /// The batches that overflows of the class's bin spilled, newest first.
    spill: Cell<*mut Batch>,
    /// Whether a refill took a spilled batch since the last maintenance step.
    unspilled: Cell<bool>,
    /// How many slabs of the class the heap holds.
    #[cfg(feature = "metrics")]
    slabs: crate::metrics::Counter,
}

/// What other threads touch in a heap slot, kept on a line of its own.
struct Shared {
    _line: [CacheLine; 0],
    claimed: AtomicBool,
    /// Single blocks and messages sent to this heap, as a push-only stack.
    inbox: AtomicPtr<Block>,
}

/// What only the claimant touches in a heap slot. The fields the fast paths read come first.
#[repr(C)]
struct Private {
    _line: [CacheLine; 0],
    id: Cell<u32>,
    refills: Cell<u32>,
    /// Set while the inbox is being read, so that nothing reads it again from inside.
    draining: Cell<bool>,
    /// How many bytes of capacity the bins have grown by, against [`GROWTH_BUDGET`].
    grown: Cell<usize>,
    /// How many bytes of blocks the spilled batches of all classes hold, against
    /// [`SPILL_BUDGET`].
    spilled: Cell<usize>,
    /// The start of the managed memory, the slab descriptors and the chunk map of the region this
    /// slot was claimed from, so that the fast paths reach them without loading the region. They
    /// are set at every claim and valid while the claim lasts.
    base: Cell<*mut u8>,
    /// The length of the managed memory in bytes, so that a free without a layout can tell a block
    /// of the region from any other pointer.
    span: Cell<usize>,
    map: Cell<*const AtomicU64>,
    slabs: Cell<*const Slab>,
    region: Cell<*const Region<'static>>,
    /// The NUMA node whose part of the region this heap takes its chunks from, and the bitmap word
    /// where single-chunk searches start.
    node: Cell<usize>,
    cursor: Cell<usize>,
    /// How many chunks this heap's slabs that have been filled once take up, which decides whether
    /// its chunks come from the zone of transparent huge pages. A slab that was never filled may
    /// have most of its pages untouched, so counting it would move heaps that are small in memory.
    held: Cell<usize>,
    /// How many chunks the blocks of whole chunks that this heap handed out and did not free take
    /// up. It counts towards the size of the heap as `held` does.
    runs: Cell<usize>,
    /// Whether the heap counted as big the last time it was asked. See [`HeapData::is_big`].
    big: Cell<bool>,
    bins: [Bin; CLASSES],
    /// This heap's bin slots in the region, set at every claim.
    slots: Cell<*mut *mut Block>,
    outboxes: [Outbox; OUTBOXES],
    kept: [Cell<Kept>; HUGE_CACHE],
    classes: [Lists; CLASSES],
    #[cfg(feature = "metrics")]
    events: crate::metrics::HeapEvents,
}

/// The state of one heap slot in a region.
///
/// Everything but `claimed` and `inbox` belongs to whichever thread has claimed the slot. An
/// all-zero value is a valid, unclaimed, empty heap.
pub(crate) struct HeapData {
    shared: Shared,
    private: Private,
}

impl fmt::Debug for HeapData {
    /// Formats only what other threads may read. The rest belongs to the claimant.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeapData")
            .field("claimed", &self.shared.claimed)
            .field("inbox", &self.shared.inbox)
            .finish_non_exhaustive()
    }
}

// SAFETY: the owner-only fields are used only by the thread that claimed the slot, and the claim
// is acquired and released with the ordering needed to hand them from one thread to the next.
unsafe impl Sync for HeapData {}

impl HeapData {
    #[cfg(any(test, feature = "testing"))]
    #[allow(clippy::missing_const_for_fn, reason = "loom atomics have no const constructors")]
    pub(crate) fn new() -> Self {
        Self {
            shared: Shared {
                _line: [],
                claimed: AtomicBool::new(false),
                inbox: AtomicPtr::new(ptr::null_mut()),
            },
            private: Private {
                _line: [],
                id: Cell::new(0),
                refills: Cell::new(0),
                draining: Cell::new(false),
                grown: Cell::new(0),
                spilled: Cell::new(0),
                base: Cell::new(ptr::null_mut()),
                span: Cell::new(0),
                map: Cell::new(ptr::null()),
                slabs: Cell::new(ptr::null()),
                region: Cell::new(ptr::null()),
                node: Cell::new(0),
                cursor: Cell::new(0),
                held: Cell::new(0),
                runs: Cell::new(0),
                big: Cell::new(false),
                bins: [const {
                    Bin {
                        top: Cell::new(ptr::null_mut()),
                        empty: Cell::new(0),
                        full: Cell::new(0),
                        capacity: Cell::new(0),
                        misses: Cell::new(0),
                        anywhere: Cell::new(false),
                        #[cfg(feature = "metrics")]
                        allocs: crate::metrics::Counter::new(),
                        #[cfg(feature = "metrics")]
                        frees: crate::metrics::Counter::new(),
                    }
                }; CLASSES],
                slots: Cell::new(ptr::null_mut()),
                outboxes: [const {
                    Outbox {
                        dest: Cell::new(0),
                        message: Cell::new(ptr::null_mut()),
                        bytes: Cell::new(0),
                    }
                }; OUTBOXES],
                kept: [const { Cell::new(Kept::NONE) }; HUGE_CACHE],
                classes: [const {
                    Lists {
                        active: Cell::new(ptr::null()),
                        active_last: Cell::new(ptr::null()),
                        full: Cell::new(ptr::null()),
                        filled: Cell::new(0),
                        spill: Cell::new(ptr::null_mut()),
                        unspilled: Cell::new(false),
                        #[cfg(feature = "metrics")]
                        slabs: crate::metrics::Counter::new(),
                    }
                }; CLASSES],
                #[cfg(feature = "metrics")]
                events: crate::metrics::HeapEvents::new(),
            },
        }
    }

    /// The allocations, frees and slabs this heap slot has counted for `class`.
    #[cfg(feature = "metrics")]
    pub(crate) fn class_counts(&self, class: Class) -> (u64, u64, u64) {
        let bin = self.bin(class);
        (bin.allocs.get(), bin.frees.get(), self.lists(class).slabs.get())
    }

    /// The slow-path counts of this heap slot.
    #[cfg(feature = "metrics")]
    pub(crate) const fn events(&self) -> &crate::metrics::HeapEvents {
        &self.private.events
    }

    /// Tries to claim this slot as heap number `id` of the region.
    pub(crate) fn claim(&self, id: u32, region: &Region<'_>) -> bool {
        let won = !self.shared.claimed.load(Ordering::Relaxed) &&
            self.shared
                .claimed
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok();
        if won {
            let private = &self.private;
            private.id.set(id);
            private.region.set(ptr::from_ref(region).cast());
            private.base.set(region.chunk_ptr(0));
            private.span.set(region.span());
            private.slabs.set(region.slabs_ptr());
            private.map.set(region.map_ptr());
            private.slots.set(region.bin_slots(id));
            self.settle_on(region, region.current_node());
            self.init_bins();
        }
        won
    }

    /// Makes the heap take its chunks from the part of the region of NUMA node `node`.
    fn settle_on(&self, region: &Region<'_>, node: usize) {
        self.private.node.set(node);
        let home = region.home(self.slab_part(region, Class::from_index(0)));
        if !home.contains(&self.private.cursor.get()) {
            self.private.cursor.set(home.start);
        }
    }

    /// Whether the heap holds enough memory that its chunks come from the zone of transparent huge
    /// pages. See [`HUGE_ZONE_CHUNKS`]. A heap that became big stays big until it holds less than
    /// a quarter of that, so that a heap whose size swings around the threshold does not leave
    /// its new slabs on small pages half of the time.
    fn is_big(&self) -> bool {
        let size = self.private.held.get() + self.private.runs.get();
        let low = if self.private.big.get() { HUGE_ZONE_CHUNKS / 4 } else { HUGE_ZONE_CHUNKS };
        let big = size >= low;
        self.private.big.set(big);
        big
    }

    /// The part of the region the next slab of `class` comes from. See [`Region::part`].
    ///
    /// Slabs of a big heap go to the zone of transparent huge pages when they will be dense. A
    /// single-chunk slab fills quickly and packs next to others. A slab of several chunks holds
    /// few blocks and may stay mostly untouched, where a huge page fault would back all of it, so
    /// it goes there only once the heap holds a slab of its class that it has filled. In a big
    /// process, see [`Region::is_big`], the slabs of a class that the heap fills go there too.
    fn slab_part(&self, region: &Region<'_>, class: Class) -> usize {
        let filled = self.lists(class).filled.get() > 0;
        let dense = class.slab_chunks() == 1 || filled;
        // In a big process, a class whose slabs this heap fills is dense whatever the heap's size,
        // so its next slabs take huge pages too. Many threads that each fill slabs of a few classes
        // would otherwise fault every page of those slabs in one by one.
        let huge = dense && (self.is_big() || (filled && region.is_big()));
        region.part(self.private.node.get(), huge)
    }

    /// Points every bin at its range of the slot array, empty and at its initial capacity. A slot
    /// that was claimed before has empty bins at the same place.
    fn init_bins(&self) {
        for (index, bin) in self.private.bins.iter().enumerate() {
            let class = Class::from_index(index);
            let end = self.bin_end(class);
            if bin.top.get().is_null() {
                bin.top.set(end);
            }
            debug_assert_eq!(bin.top.get(), end, "a released heap left blocks in a bin");
            bin.empty.set(end.addr() as u16);
            bin.misses.set(0);
            bin.anywhere.set(reusable_anywhere(class));
            self.set_capacity(class, class.bin_initial());
        }
        self.private.grown.set(0);
    }

    /// The end of the bin of `class` in the slot array, past its last slot.
    const fn bin_end(&self, class: Class) -> *mut *mut Block {
        self.slots().wrapping_add(class.bin_start() + class.bin_max())
    }

    /// Sets how many blocks the bin of `class` may hold. It must hold no more than that already.
    fn set_capacity(&self, class: Class, capacity: usize) {
        let bin = self.bin(class);
        // A bin that held more than its capacity would let pushes run past its start.
        assert!(self.bin_len(class) <= capacity && capacity <= class.bin_max());
        bin.capacity.set(capacity as u16);
        bin.full.set(self.bin_end(class).wrapping_sub(capacity).addr() as u16);
    }

    /// Records that the bin of `class` missed in the way `kind` says, and grows it if the class
    /// alternates between running dry and overflowing, which is how TCMalloc sizes its per-CPU
    /// caches. Returns whether the bin grew.
    fn note_miss(&self, class: Class, kind: u8) -> bool {
        let bin = self.bin(class);
        let last = bin.misses.get() & (UNDERFLOW | OVERFLOW);
        let refilled = if kind == UNDERFLOW { REFILLED } else { bin.misses.get() & REFILLED };
        bin.misses.set(kind | MISSED | refilled);
        let capacity = usize::from(bin.capacity.get());
        if last == 0 || last == kind || capacity == class.bin_max() {
            return false;
        }
        let grown = (capacity * 2).min(class.bin_max());
        let bytes = (grown - capacity) * class.block_size();
        if self.private.grown.get() + bytes > GROWTH_BUDGET {
            return false;
        }
        self.private.grown.set(self.private.grown.get() + bytes);
        self.set_capacity(class, grown);
        count!(self, BinsGrown, 1);
        true
    }

    /// The first of this heap's bin slots, with provenance over all of them. They live in the
    /// region and not in the heap slot, so that the heap slot stays small.
    const fn slots(&self) -> *mut *mut Block {
        self.private.slots.get()
    }

    /// Hands over everything this heap holds that belongs to others and gives the slot up for
    /// another thread to claim.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    pub(crate) unsafe fn give_up(&self) {
        // SAFETY: forwarded.
        unsafe { self.empty_caches(self.region()) };
        self.release_claim();
    }

    /// Gives the slot up for another thread to claim.
    pub(crate) fn release_claim(&self) {
        self.shared.claimed.store(false, Ordering::Release);
    }

    pub(crate) fn is_claimed(&self) -> bool {
        self.shared.claimed.load(Ordering::Relaxed)
    }

    /// Whether anything waits in the inbox.
    pub(crate) fn has_mail(&self) -> bool {
        !self.shared.inbox.load(Ordering::Relaxed).is_null()
    }

    /// Sends this heap one block of its own slabs, from any thread.
    ///
    /// # Safety
    ///
    /// `block` must be an in-use block of a slab owned by this heap, which the caller gives up.
    pub(crate) unsafe fn post_block(&self, block: *mut Block) {
        // SAFETY: forwarded.
        unsafe { self.post(block.map_addr(|addr| addr | SINGLE), block) };
    }

    /// Pushes an inbox node, whose untagged address is `raw`, onto this heap's inbox.
    ///
    /// # Safety
    ///
    /// `raw` must be a block that the caller owns and gives up, whose first word may be
    /// overwritten.
    unsafe fn post(&self, node: *mut Block, raw: *mut Block) {
        let inbox = &self.shared.inbox;
        let mut head = inbox.load(Ordering::Relaxed);
        loop {
            // SAFETY: the node is ours until the compare-and-swap publishes it.
            unsafe { (*raw).next = head };
            match inbox.compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    const fn lists(&self, class: Class) -> &Lists {
        &self.private.classes[class.index()]
    }

    const fn head(&self, class: Class, list: List) -> &Cell<*const Slab> {
        let lists = self.lists(class);
        match list {
            List::Active => &lists.active,
            List::Full => &lists.full,
            List::None => unreachable!(),
        }
    }

    /// The region this slot is claimed from.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    #[inline(always)]
    const unsafe fn region(&self) -> &Region<'_> {
        // SAFETY: the claim was made on this region, which outlives every claim on its slots.
        unsafe { &*self.private.region.get() }
    }

    /// The chunk map entry of the chunk that holds `ptr`.
    #[inline(always)]
    fn entry_of(&self, ptr: *const u8) -> Entry {
        let index = (ptr.addr() - self.private.base.get().addr()) >> CHUNK_SHIFT;
        // SAFETY: the block lies in the region, which has an entry for every chunk.
        Entry(unsafe { &*self.private.map.get().add(index) }.load(Ordering::Relaxed))
    }

    /// The bin of `class`. Classes are always in range, so the fast paths skip the bounds check.
    #[inline(always)]
    fn bin(&self, class: Class) -> &Bin {
        debug_assert!(class.index() < CLASSES);
        // SAFETY: every class index is below `CLASSES`, the length of the array.
        unsafe { self.private.bins.get_unchecked(class.index()) }
    }

    /// Pops a block off the bin of `class`.
    #[inline(always)]
    fn pop(&self, class: Class) -> Option<*mut Block> {
        Self::pop_from(self.bin(class))
    }

    /// Pops a block off `bin`.
    #[inline(always)]
    fn pop_from(bin: &Bin) -> Option<*mut Block> {
        let top = bin.top.get();
        if top.addr() as u16 == bin.empty.get() {
            return None;
        }
        bin.top.set(top.wrapping_add(1));
        #[cfg(feature = "metrics")]
        bin.allocs.add(1);
        // SAFETY: slots between the top and the end of a bin hold blocks, which are never null.
        // Saying so lets callers that turn the block into a nullable pointer skip a check.
        unsafe {
            let block = *top;
            core::hint::assert_unchecked(!block.is_null());
            Some(block)
        }
    }

    /// Pushes a block onto the bin of `class`, or returns false if the bin is full.
    #[inline(always)]
    fn push(&self, class: Class, block: *mut Block) -> bool {
        Self::push_to(self.bin(class), block)
    }

    /// Pushes a block onto `bin`, or returns false if the bin is full.
    #[inline(always)]
    fn push_to(bin: &Bin, block: *mut Block) -> bool {
        let top = bin.top.get();
        if top.addr() as u16 == bin.full.get() {
            return false;
        }
        let top = top.wrapping_sub(1);
        // SAFETY: the bin is not full, so the slot below the top is inside it.
        unsafe { *top = block };
        bin.top.set(top);
        true
    }

    /// How many blocks the bin of `class` holds.
    fn bin_len(&self, class: Class) -> usize {
        let end = self.bin_end(class);
        (end.addr() - self.bin(class).top.get().addr()) / size_of::<usize>()
    }

    /// Allocates memory for the layout, returning null if the region is out of room or the layout
    /// is not supported. Common requests take a table lookup and a pop from a bin.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(always)]
    pub(crate) unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Class::is_table_request(layout) {
            // SAFETY: forwarded, and the table covers the size.
            return unsafe { self.alloc_table(layout.size(), layout.align() > STEP) };
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_general(layout) }
    }
    /// Allocates `size` bytes for a size the class table covers, zero included, aligned to 16 if
    /// `wide` and to 8 otherwise. A wide request rounds the size up to a multiple of 16, whose
    /// class's blocks are 16-byte aligned.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot, and `size` must be at most [`TABLE_MAX`].
    #[inline(always)]
    pub(crate) unsafe fn alloc_table(&self, size: usize, wide: bool) -> *mut u8 {
        if let Some(block) = self.pop_table(size, wide) {
            return block;
        }
        // SAFETY: forwarded.
        unsafe { self.refill(Class::from_table(Self::table_step(size, wide) * STEP)) }.cast()
    }

    /// Pops a block of `size` bytes, as [`alloc_table`](Self::alloc_table) places it, off its bin,
    /// or returns None if the bin is empty.
    ///
    /// The caller must hold the claim on this slot, and `size` must be at most [`TABLE_MAX`].
    #[inline(always)]
    pub(crate) fn pop_table(&self, size: usize, wide: bool) -> Option<*mut u8> {
        debug_assert!(size <= TABLE_MAX);
        let step = Self::table_step(size, wide);
        // SAFETY: the size is at most `TABLE_MAX`, so its step is in the table, and every offset in
        // it is that of a bin in the array.
        let bin = unsafe {
            let offset = *BIN_OFFSETS.get_unchecked(step);
            &*self.private.bins.as_ptr().byte_add(usize::from(offset))
        };
        Self::pop_from(bin).map(<*mut Block>::cast)
    }

    /// The index in the class table of a request of `size` bytes. A wide request rounds the size
    /// up to a multiple of 16, whose class's blocks are 16-byte aligned.
    #[inline(always)]
    const fn table_step(size: usize, wide: bool) -> usize {
        if wide { size.div_ceil(2 * STEP) * 2 } else { size.div_ceil(STEP) }
    }

    /// Allocates anything that the fast path in [`HeapData::alloc`] does not take.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(never)]
    unsafe fn alloc_general(&self, layout: Layout) -> *mut u8 {
        match classify(layout) {
            Some(Request::Small(class)) => {
                // Bins serve most of a quiet program's requests, and then nothing runs the purge
                // clock, which keeps memory that a burst left behind. Bigger blocks come through
                // here, which costs a load of a flag, and runs the clock when chunks are waiting.
                // SAFETY: the claim keeps the region alive.
                unsafe { self.region() }.purge_if_due();
                self.pop(class)
                    // SAFETY: forwarded.
                    .unwrap_or_else(|| unsafe { self.refill(class) })
                    .cast()
            }
            // SAFETY: forwarded.
            Some(Request::Huge { chunks }) => unsafe { self.alloc_whole(chunks) },
            None => ptr::null_mut(),
        }
    }

    /// Allocates a block of whole chunks, out of line so that the saves of registers that it needs
    /// stay off the path of small blocks in [`alloc_general`](Self::alloc_general).
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(never)]
    unsafe fn alloc_whole(&self, chunks: usize) -> *mut u8 {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let block = self.alloc_huge(region, chunks);
        if !block.is_null() {
            return block;
        }
        // The region looks full, but this heap may be sitting on empty slabs. Give them back and
        // retry.
        // SAFETY: the caller holds the claim.
        unsafe { self.flush(region) };
        self.alloc_huge(region, chunks)
    }

    /// Fills the empty bin of `class` and pops a block, or returns null if the region is out of
    /// room.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[cold]
    #[inline(never)]
    unsafe fn refill(&self, class: Class) -> *mut Block {
        count!(self, Refills, 1);
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        // SAFETY: the caller holds the claim.
        if unsafe { self.read_mail(region) } &&
            let Some(block) = self.pop(class)
        {
            return block;
        }
        // SAFETY: as above.
        unsafe { self.maintain(region) };
        // Maintenance may have allocated a message from this very class.
        if let Some(block) = self.pop(class) {
            return block;
        }
        self.note_miss(class, UNDERFLOW);
        if self.unspill(region, class) &&
            let Some(block) = self.pop(class)
        {
            return block;
        }
        if self.fill(region, class) == 0 {
            // The region looks full, but this heap may be sitting on empty slabs. Give them back
            // and retry.
            // SAFETY: as above.
            unsafe { self.flush(region) };
            if self.fill(region, class) == 0 {
                return ptr::null_mut();
            }
        }
        self.pop(class).unwrap_or(ptr::null_mut())
    }

    /// Runs the clocks of a refill: the purge clock of the region, and every few refills a step
    /// that posts what the outboxes hold.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn maintain(&self, region: &Region<'_>) {
        region.purge_if_due();
        let refills = self.private.refills.get().wrapping_add(1);
        self.private.refills.set(refills);
        if refills.is_multiple_of(MAINTENANCE_INTERVAL) {
            for outbox in &self.private.outboxes {
                // SAFETY: forwarded.
                unsafe { self.post_outbox(region, outbox) };
            }
            // Huge blocks that nobody asked for since the last step go back to the region.
            for kept in &self.private.kept {
                let block = kept.get();
                if block.chunks != 0 && !block.reused {
                    kept.set(Kept::NONE);
                    region.free_chunks(block.first, block.chunks);
                } else {
                    kept.set(Kept { reused: false, ..block });
                }
            }
            for (index, bin) in self.private.bins.iter().enumerate() {
                let class = Class::from_index(index);
                let capacity = usize::from(bin.capacity.get());
                if bin.misses.get() & MISSED == 0 && capacity > class.bin_initial() {
                    // A bin that has not missed for a while holds more than it needs.
                    let shrunk = (capacity / 2).max(class.bin_initial());
                    // SAFETY: forwarded. The bin holds no more than the new capacity afterwards,
                    // even when giving blocks back filed mail into it.
                    unsafe { self.flush_bin(class, shrunk) };
                    self.set_capacity(class, shrunk);
                    count!(self, BinsShrunk, 1);
                    let bytes = (capacity - shrunk) * class.block_size();
                    self.private.grown.set(self.private.grown.get() - bytes);
                }
                // A bin that has not missed for a whole interval forgets that it was refilled.
                let misses = bin.misses.get();
                bin.misses.set(if misses & MISSED == 0 {
                    misses & !REFILLED
                } else {
                    misses & !MISSED
                });
                // Batches that no refill wanted since the last step go back to their slabs.
                let lists = self.lists(class);
                if !lists.unspilled.replace(false) && !lists.spill.get().is_null() {
                    // SAFETY: forwarded.
                    unsafe { self.return_spill(region, class, false) };
                }
            }
        }
    }

    /// Moves blocks from the active slabs of `class` into its bin, which must be empty, until the
    /// bin is half full. Returns how many it moved, which is zero only if the region is out of
    /// room. The blocks come out of each slab in address order, and the bin hands them out in
    /// the same order.
    fn fill(&self, region: &Region<'_>, class: Class) -> usize {
        let bin = self.bin(class);
        let want = (usize::from(bin.capacity.get()) / 2).max(1);
        // The bin is empty, so its top is its end. Blocks are written upwards from `want` slots
        // below the end, lowest address first, so that the bin hands them out in address order.
        let end = bin.top.get();
        let first = end.wrapping_sub(want);
        let mut got = 0;
        while got < want {
            // SAFETY: slabs on our lists are ours.
            let Some(slab) = (unsafe { self.lists(class).active.get().as_ref() }) else {
                // SAFETY: the caller holds the claim.
                if unsafe { self.add_slab(region, class) } {
                    continue;
                }
                break;
            };
            // SAFETY: as above.
            let slab = unsafe { slab.owned() };
            let mut slot = first.wrapping_add(got);
            got += slab.take(want - got, |block| {
                // SAFETY: the slot lies in the bin, below its end.
                unsafe { *slot = block.cast() };
                slot = slot.wrapping_add(1);
            });
            self.after_take(slab, class);
        }
        let top = end.wrapping_sub(got);
        if got < want {
            // SAFETY: both ranges lie in the bin.
            unsafe { ptr::copy(first, top, got) };
        }
        bin.top.set(top);
        got
    }

    /// Takes a fresh slab of `class` from the chunk allocator and puts it at the front of the
    /// active list. Returns false if the region has no room.
    unsafe fn add_slab(&self, region: &Region<'_>, class: Class) -> bool {
        let chunks = class.slab_chunks();
        let part = self.slab_part(region, class);
        let home = region.home(part);
        let cursor = self.private.cursor.get();
        let cursor = if home.contains(&cursor) { cursor } else { home.start };
        let Some((first, zeroed)) = region.alloc_chunks_zeroed(chunks, chunks, cursor, part) else {
            return false;
        };
        count!(self, SlabsAdded, 1);
        #[cfg(feature = "metrics")]
        self.lists(class).slabs.add(1);
        region.set_owner(first, chunks, self.private.id.get(), class);
        let slab = region.slab(first);
        // SAFETY: the chunks were just allocated to us, so nobody else uses this descriptor.
        let owned = unsafe { slab.owned() };
        // SAFETY: the chunks are a slab-sized, slab-aligned run that is ours alone.
        unsafe { owned.init(class, region.chunk_ptr(first), zeroed) };
        self.push_front(owned, List::Active);
        if chunks == 1 && home.contains(&(first / 64)) {
            self.private.cursor.set(first / 64);
        }
        true
    }

    /// Moves a slab that blocks were just taken from to the full list if it has none left.
    fn after_take(&self, slab: Owned<'_>, class: Class) {
        if slab.is_exhausted() {
            self.move_to(slab, List::Full);
            if slab.mark_filled() {
                self.private.held.set(self.private.held.get() + class.slab_chunks());
                let filled = &self.lists(class).filled;
                filled.set(filled.get() + 1);
            }
        }
    }

    /// Allocates zeroed memory for the layout, returning null if the region is out of room or the
    /// layout is not supported.
    ///
    /// Fresh memory already reads as zeros, so it is not written again, as mimalloc and jemalloc
    /// skip zeroing pages they know are fresh. A huge block knows from the chunk bitmap. A block
    /// of a slab is fresh if the slab was set up on zeroed memory and the block lies beyond every
    /// block the slab has handed out. Blocks in a bin may have been used, so a request that a bin
    /// can serve takes the block from the bin and clears it, while it is likely still in the cache.
    /// When the bin is empty the block comes straight from a slab instead, where it may be fresh.
    /// Small blocks are cleared without asking, which costs less than finding out.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    pub(crate) unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        match classify(layout) {
            Some(Request::Small(class)) if class.block_size() >= ZERO_CHECK_MIN => {
                let (block, zeroed) = self.pop(class).map_or_else(
                    // SAFETY: forwarded.
                    || unsafe { self.take_zeroed(class) },
                    |block| (block.cast::<u8>(), false),
                );
                if !block.is_null() && !zeroed {
                    // SAFETY: the block holds at least `size` bytes and is ours.
                    unsafe { block.write_bytes(0, size) };
                }
                block
            }
            Some(Request::Huge { chunks }) => {
                // SAFETY: the claim keeps the region alive.
                let region = unsafe { self.region() };
                let (mut block, mut zeroed) = self.alloc_huge_zeroed(region, chunks);
                if block.is_null() {
                    // SAFETY: the caller holds the claim.
                    unsafe { self.flush(region) };
                    (block, zeroed) = self.alloc_huge_zeroed(region, chunks);
                }
                if !block.is_null() && !zeroed {
                    // SAFETY: as above.
                    unsafe { block.write_bytes(0, size) };
                }
                block
            }
            _ => {
                // SAFETY: forwarded.
                let block = unsafe { self.alloc(layout) };
                if !block.is_null() {
                    // SAFETY: as above.
                    unsafe { block.write_bytes(0, size) };
                }
                block
            }
        }
    }

    /// Takes one block of `class` straight from a slab for a zeroed request whose bin is empty,
    /// and tells whether it still holds the zeros of fresh memory. Falls back to an ordinary
    /// refill when the inbox has blocks to take in first.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    #[cold]
    unsafe fn take_zeroed(&self, class: Class) -> (*mut u8, bool) {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        if self.has_mail() {
            // SAFETY: the caller holds the claim.
            return (unsafe { self.refill(class) }.cast(), false);
        }
        // SAFETY: as above.
        unsafe { self.maintain(region) };
        if let Some(block) = self.pop(class) {
            return (block.cast(), false);
        }
        loop {
            // SAFETY: slabs on our lists are ours.
            if let Some(slab) = unsafe { self.lists(class).active.get().as_ref() } {
                // SAFETY: as above.
                let slab = unsafe { slab.owned() };
                if let Some((block, zeroed)) = slab.take_one() {
                    self.after_take(slab, class);
                    #[cfg(feature = "metrics")]
                    {
                        self.bin(class).allocs.add(1);
                        if zeroed {
                            count!(self, ZeroedFresh, 1);
                        }
                    }
                    return (block, zeroed);
                }
            }
            // SAFETY: as above.
            if !unsafe { self.add_slab(region, class) } {
                // SAFETY: as above.
                return (unsafe { self.refill(class) }.cast(), false);
            }
        }
    }

    /// Resizes a block, returning null and leaving the block untouched if there is no room.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot, the block must be live with exactly the old
    /// layout, and the new size must be non-zero and not overflow when rounded up to the
    /// alignment.
    pub(crate) unsafe fn realloc(&self, ptr: NonNull<u8>, old: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees the size is valid for the alignment.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, old.align()) };
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        match (classify(old), classify(new)) {
            (Some(Request::Small(_)), Some(Request::Small(_))) => {
                // The block keeps its place while the new size fits and wastes at most half of
                // it, as mimalloc decides. Frees read the class from the chunk map and not from
                // the layout, so the block is still freed as what it is.
                let size = self.entry_of(ptr.as_ptr()).class().block_size();
                if new_size <= size && new_size >= size / 2 {
                    return ptr.as_ptr();
                }
            }
            (Some(Request::Huge { chunks: had }), Some(Request::Huge { chunks: wants })) => {
                let first = region.chunk_index(ptr.as_ptr());
                if wants <= had {
                    if wants < had {
                        count!(self, HugeBytesFreed, (had - wants) << CHUNK_SHIFT);
                        region.set_huge(first, wants);
                        region.free_chunks(first + wants, had - wants);
                        region.purge_if_due();
                    }
                    return ptr.as_ptr();
                }
                if region.grow_in_place(first, had, wants) {
                    count!(self, HugeBytesAllocated, (wants - had) << CHUNK_SHIFT);
                    region.set_huge(first, wants);
                    return ptr.as_ptr();
                }
            }
            _ => {}
        }
        // SAFETY: forwarded.
        let block = unsafe { self.alloc(new) };
        if block.is_null() {
            return block;
        }
        // SAFETY: both blocks are live and distinct, and the smaller size fits in each.
        unsafe {
            ptr::copy_nonoverlapping(ptr.as_ptr(), block, old.size().min(new_size));
            self.dealloc(ptr, old);
        }
        block
    }

    /// Frees a block.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot, and the block must have been allocated from
    /// this region with exactly the layout and not be used again.
    #[inline(always)]
    pub(crate) unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() <= MAX_SMALL {
            // SAFETY: forwarded.
            unsafe { self.free_small(ptr.as_ptr().cast()) };
        } else {
            // SAFETY: forwarded.
            unsafe { self.free_huge(ptr, layout) };
        }
    }

    /// Frees a huge block. A block of up to [`HUGE_CACHE_BLOCK`] is kept for the next request of
    /// its size if there is room, so a program that frees and allocates big buffers in a loop does
    /// not pay for the bitmap and the purge clock each time, as jemalloc and mimalloc keep large
    /// extents and segments around for a while.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc), and the layout must be a huge one.
    #[inline(never)]
    unsafe fn free_huge(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let Some(Request::Huge { chunks }) = classify(layout) else {
            // SAFETY: forwarded.
            return unsafe { region.dealloc(ptr, layout) };
        };
        let bytes = chunks << CHUNK_SHIFT;
        self.private.runs.set(self.private.runs.get().saturating_sub(chunks));
        count!(self, HugeFrees, 1);
        count!(self, HugeBytesFreed, bytes);
        if bytes <= HUGE_CACHE_BLOCK {
            let held: usize =
                self.private.kept.iter().map(|kept| kept.get().chunks << CHUNK_SHIFT).sum();
            let empty = self.private.kept.iter().find(|kept| kept.get().chunks == 0);
            if let Some(empty) = empty &&
                held + bytes <= HUGE_CACHE_BYTES
            {
                let first = region.chunk_index(ptr.as_ptr());
                // A block kept now survives the next maintenance step, and goes at the one after
                // unless it is asked for in between.
                empty.set(Kept { first, chunks, reused: true });
                return;
            }
        }
        region.free_chunks(region.chunk_index(ptr.as_ptr()), chunks);
        region.purge_if_due();
    }

    /// Frees a block of a slab. Its class and its owner both come from one load of the chunk map,
    /// so the layout does not matter beyond telling slab blocks from huge ones, and a block that a
    /// realloc kept in place is freed as what it is.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc).
    #[inline(always)]
    unsafe fn free_small(&self, block: *mut Block) {
        // SAFETY: forwarded.
        unsafe { self.free_entry(block, self.entry_of(block.cast())) };
    }

    /// Frees a block without its layout, as C's `free` does, and ignores a pointer outside the
    /// region. The chunk map tells a slab's block from a huge one, so this takes one load of the
    /// map where looking the layout up first and then freeing would take two.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot, and a pointer inside the region must be the
    /// start of a live block that is not used again.
    #[inline(always)]
    pub(crate) unsafe fn free_unsized(&self, ptr: NonNull<u8>) {
        let Some(entry) = self.entry_in_region(ptr.as_ptr()) else { return };
        // SAFETY: forwarded, and the entry is the block's.
        unsafe { self.free_with_entry(ptr, entry) };
    }

    /// Frees the block at `ptr`, of a slab or huge, whose chunk map entry is `entry`.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot, `entry` must be the entry of the block's chunk,
    /// and the block must be live and not used again.
    #[inline(always)]
    pub(crate) unsafe fn free_with_entry(&self, ptr: NonNull<u8>, entry: Entry) {
        if let Some(chunks) = entry.huge_chunks() {
            // SAFETY: a huge block of `chunks` chunks has this layout, and the caller gives it up.
            return unsafe {
                self.free_huge(ptr, Layout::from_size_align_unchecked(chunks << CHUNK_SHIFT, 16))
            };
        }
        // SAFETY: the caller gives up a block of a slab.
        unsafe { self.free_entry(ptr.as_ptr().cast(), entry) };
    }

    /// The chunk map entry of the chunk that holds `ptr`, or `None` if `ptr` is not in the region.
    #[inline(always)]
    fn entry_in_region(&self, ptr: *const u8) -> Option<Entry> {
        let offset = ptr.addr().wrapping_sub(self.private.base.get().addr());
        (offset < self.private.span.get()).then(|| self.entry_of(ptr))
    }

    /// How many bytes the block at `ptr` holds, or `None` if `ptr` is not in the region. The
    /// answer is only meaningful for the start of a live block.
    #[cfg(all(feature = "std", not(loom)))]
    #[inline(always)]
    pub(crate) fn usable_size(&self, ptr: NonNull<u8>) -> Option<usize> {
        let entry = self.entry_in_region(ptr.as_ptr())?;
        Some(entry.huge_chunks().map_or_else(|| entry.class().block_size(), |n| n << CHUNK_SHIFT))
    }

    /// Frees a block of a slab whose chunk map entry is `entry`.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc), and `entry` must be the entry of the block's chunk.
    #[inline(always)]
    pub(crate) unsafe fn free_entry(&self, block: *mut Block, entry: Entry) {
        // SAFETY: entries hold the offset of a bin in the array.
        let bin = unsafe { &*self.private.bins.as_ptr().byte_add(entry.bin_offset()) };
        #[cfg(feature = "metrics")]
        bin.frees.add(1);
        // Testing the owner first keeps the common case, a block of this heap, on one branch that
        // predicts well. Whether the class allows reuse depends on the size, which is as random as
        // the program's requests.
        if entry.owner() != self.private.id.get() && !bin.anywhere.get() {
            // SAFETY: forwarded.
            return unsafe { self.send(block, entry.class(), entry.owner()) };
        }
        if !Self::push_to(bin, block) {
            // SAFETY: forwarded.
            unsafe { self.free_full(block, entry.class()) };
        }
    }

    /// Frees a block whose bin is full, by giving the older half of the bin back first.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be an in-use block of `class` that is
    /// given up.
    #[cold]
    #[inline(never)]
    unsafe fn free_full(&self, block: *mut Block, class: Class) {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        // A thread that only frees never refills, so this is where it takes in its mail.
        // SAFETY: the caller holds the claim.
        unsafe { self.read_mail(region) };
        if self.note_miss(class, OVERFLOW) && self.push(class, block) {
            return;
        }
        if self.spill(region, class) && self.push(class, block) {
            return;
        }
        loop {
            let keep = usize::from(self.bin(class).capacity.get()) / 2;
            // SAFETY: forwarded.
            unsafe { self.flush_bin(class, keep) };
            if self.push(class, block) {
                return;
            }
        }
    }

    /// Takes all but the `keep` newest blocks out of the bin of `class` and gives them back to
    /// their slabs or their owners.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn flush_bin(&self, class: Class, keep: usize) {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let bin = self.bin(class);
        let len = self.bin_len(class);
        if len <= keep {
            return;
        }
        // The oldest blocks are at the end of the bin. Move them out first and the rest down, so
        // that the bin is consistent if giving the blocks back allocates or frees again. A bin
        // can be longer than the buffer, so this goes a buffer at a time.
        let mut out = [ptr::null_mut::<Block>(); FLUSH_BATCH];
        let mut len = len;
        while len > keep {
            let gone = (len - keep).min(FLUSH_BATCH);
            let top = bin.top.get();
            // SAFETY: the bin holds `len` blocks from `top`, and `out` holds `gone` of them.
            unsafe {
                ptr::copy_nonoverlapping(top.add(len - gone), out.as_mut_ptr(), gone);
                ptr::copy(top, top.add(gone), len - gone);
            }
            bin.top.set(top.wrapping_add(gone));
            // SAFETY: the blocks were in the bin, so they are in use and ours to give back.
            unsafe { self.give_back(region, class, &out[..gone], false) };
            // Giving back may have filed blocks from the inbox into this bin.
            len = self.bin_len(class);
        }
    }

    /// Gives blocks of `class` that this heap holds back: blocks of its own slabs to the slab, and
    /// the others to their owners, through the outboxes or, if `directly`, on their own. Blocks
    /// that follow each other in one slab look the slab up once.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the blocks must be distinct in-use blocks of `class`
    /// that are given up.
    unsafe fn give_back(
        &self,
        region: &Region<'_>,
        class: Class,
        blocks: &[*mut Block],
        directly: bool,
    ) {
        let id = self.private.id.get();
        let span = class.slab_chunks() << CHUNK_SHIFT;
        let mut run: Option<Returns<'_>> = None;
        let mut rest = blocks;
        while let Some((&block, more)) = rest.split_first() {
            rest = more;
            if let Some(returns) = &mut run {
                if block.addr().wrapping_sub(returns.slab().start().addr()) < span {
                    // SAFETY: the block is an in-use block of the slab, given up.
                    unsafe { returns.add(block) };
                    continue;
                }
                if let Some(returns) = run.take() {
                    self.settle(region, returns.finish());
                }
            }
            let owner = self.entry_of(block.cast()).owner();
            if owner != id {
                let at = blocks.len() - rest.len() - 1;
                // SAFETY: forwarded, and the first of the blocks is `block`, which is `owner`'s.
                let given =
                    unsafe { self.give_away(region, class, &blocks[at..], owner, directly) };
                rest = &blocks[at + given..];
                continue;
            }
            // SAFETY: the chunk map says the slab is ours.
            let slab =
                unsafe { region.slab_of(NonNull::new_unchecked(block.cast()), class).owned() };
            let mut returns = Returns::new(slab);
            // SAFETY: the block is in use and given up.
            unsafe { returns.add(block) };
            run = Some(returns);
        }
        if let Some(returns) = run {
            self.settle(region, returns.finish());
        }
    }

    /// Gives the first of `blocks`, which belongs to heap `owner`, back to it, with the blocks
    /// after it that belong to it too, and returns how many blocks it gave. This stays out of line
    /// so that the loop of [`give_back`](Self::give_back) over the heap's own blocks stays short.
    ///
    /// # Safety
    ///
    /// As for [`give_back`](Self::give_back), and the first block must belong to `owner`.
    #[inline(never)]
    unsafe fn give_away(
        &self,
        region: &Region<'_>,
        class: Class,
        blocks: &[*mut Block],
        owner: u32,
        directly: bool,
    ) -> usize {
        // The blocks that follow and belong to the same owner go with the first. Slabs are aligned
        // to their size, so blocks of one slab need no second look at the map.
        let mask = !((class.slab_chunks() << CHUNK_SHIFT) - 1);
        let mut slab = blocks[0].addr() & mask;
        let mut len = 1;
        while let Some(&next) = blocks.get(len) {
            if next.addr() & mask != slab {
                if self.entry_of(next.cast()).owner() != owner {
                    break;
                }
                slab = next.addr() & mask;
            }
            len += 1;
        }
        let others = &blocks[..len];
        if directly {
            for &block in others {
                // SAFETY: the block belongs to `owner` and is given up.
                unsafe { region.heap_slot(owner).post_block(block) };
            }
        } else {
            // SAFETY: as above.
            unsafe { self.send_run(others, class, owner) };
        }
        len
    }

    /// Moves or releases a slab that has just taken blocks back, as its occupancy says.
    fn settle(&self, region: &Region<'_>, slab: Owned<'_>) {
        if slab.list() == List::Full {
            // It has free blocks again, so it belongs where refills look.
            self.move_to(slab, List::Active);
        }
        if slab.used() == 0 {
            self.retire(region, slab);
        }
    }

    /// Returns a block of one of this heap's slabs to the slab, and moves or releases the slab as
    /// its occupancy says.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be an in-use block of one of this heap's
    /// slabs of `class`, which is given up.
    unsafe fn free_to_slab(&self, region: &Region<'_>, block: *mut Block, class: Class) {
        // SAFETY: the chunk map says the slab is ours, and the caller gives the block up.
        let slab = unsafe {
            let slab = region.slab_of(NonNull::new_unchecked(block.cast()), class).owned();
            slab.push(block);
            slab
        };
        self.settle(region, slab);
    }

    /// Appends a block of another heap's slab to the outbox for that heap.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be an in-use block of `class` of a slab
    /// owned by heap `owner`, which is given up.
    #[cold]
    #[inline(never)]
    unsafe fn send(&self, block: *mut Block, class: Class, owner: u32) {
        count!(self, BlocksSent, 1);
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let outbox = &self.private.outboxes[owner as usize % OUTBOXES];
        // SAFETY: the caller holds the claim.
        let message = unsafe { self.open_outbox(region, outbox, class, owner) };
        if message.is_null() {
            // SAFETY: the caller gives the block up, and it belongs to `owner`.
            return unsafe { region.heap_slot(owner).post_block(block) };
        }
        // SAFETY: an open message is ours until it is posted, and it is never left full.
        let count = unsafe {
            let count = (*message).count as usize;
            (&raw mut (*message).blocks).cast::<*mut Block>().add(count).write(block);
            (*message).count = (count + 1) as u32;
            count + 1
        };
        let bytes = outbox.bytes.get() as usize + class.block_size();
        outbox.bytes.set(bytes as u32);
        if count == MESSAGE_BLOCKS || bytes >= OUTBOX_BYTES {
            // SAFETY: the caller holds the claim.
            unsafe { self.post_outbox(region, outbox) };
        }
    }

    /// Appends blocks of `class` that all belong to heap `owner` to the outbox for that heap,
    /// posting the message each time it fills, so that a run of blocks costs one look at the
    /// outbox per message and not a call per block. A single block from a free takes
    /// [`send`](Self::send), which is shorter for one block.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the blocks must be distinct in-use blocks of `class` of
    /// slabs owned by heap `owner`, which are given up.
    unsafe fn send_run(&self, blocks: &[*mut Block], class: Class, owner: u32) {
        count!(self, BlocksSent, blocks.len());
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let size = class.block_size();
        let outbox = &self.private.outboxes[owner as usize % OUTBOXES];
        let mut rest = blocks;
        while !rest.is_empty() {
            // SAFETY: the caller holds the claim.
            let message = unsafe { self.open_outbox(region, outbox, class, owner) };
            if message.is_null() {
                for &block in rest {
                    // SAFETY: the caller gives the block up, and it belongs to `owner`.
                    unsafe { region.heap_slot(owner).post_block(block) };
                }
                return;
            }
            // SAFETY: an open message is ours until it is posted, and it is never left full.
            let mut count = unsafe { (*message).count as usize };
            let mut bytes = outbox.bytes.get() as usize;
            // Up to the block that fills the message or takes it past its byte limit.
            while let Some((&block, more)) = rest.split_first() &&
                count < MESSAGE_BLOCKS &&
                bytes < OUTBOX_BYTES
            {
                // SAFETY: the message has room for the block, as the loop condition says.
                unsafe {
                    (&raw mut (*message).blocks).cast::<*mut Block>().add(count).write(block)
                };
                count += 1;
                bytes += size;
                rest = more;
            }
            // SAFETY: as above.
            unsafe { (*message).count = count as u32 };
            outbox.bytes.set(bytes as u32);
            if count == MESSAGE_BLOCKS || bytes >= OUTBOX_BYTES {
                // SAFETY: the caller holds the claim.
                unsafe { self.post_outbox(region, outbox) };
            }
        }
    }

    /// Readies `outbox` to take blocks of `class` for heap `owner` and returns its message, or null
    /// if the blocks must travel on their own because they are too big to wait in an outbox or
    /// there is no memory for a message.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn open_outbox(
        &self,
        region: &Region<'_>,
        outbox: &Outbox,
        class: Class,
        owner: u32,
    ) -> *mut Message {
        if outbox.message.get().is_null() {
            // Messages come back to their sender once read, and a thread that only frees would
            // otherwise never take them in and keep allocating new ones. Reading the inbox may
            // send blocks too, so the outbox is looked at only after it.
            // SAFETY: the caller holds the claim.
            unsafe { self.read_mail(region) };
        }
        let mut message = outbox.message.get();
        if !message.is_null() && outbox.dest.get() != owner {
            // SAFETY: the caller holds the claim.
            unsafe { self.post_outbox(region, outbox) };
            message = outbox.message.get();
        }
        if message.is_null() {
            if class.block_size() >= OUTBOX_BYTES {
                return ptr::null_mut();
            }
            message = self.alloc_message(region);
            if message.is_null() {
                return message;
            }
            // SAFETY: the message is a fresh block of ours.
            unsafe { (*message).count = 0 };
            outbox.message.set(message);
            outbox.bytes.set(0);
        }
        outbox.dest.set(owner);
        // SAFETY: the message is ours until it is posted.
        unsafe {
            if (*message).count == 0 {
                (*message).class = class.index() as u32;
            } else if (*message).class != class.index() as u32 {
                (*message).class = MIXED;
            }
        }
        message
    }

    /// Posts what an outbox holds to its destination. A message with one block sends the block on
    /// its own and stays for reuse.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn post_outbox(&self, region: &Region<'_>, outbox: &Outbox) {
        let message = outbox.message.get();
        if message.is_null() {
            return;
        }
        let dest = region.heap_slot(outbox.dest.get());
        outbox.bytes.set(0);
        // SAFETY: the message is ours until it is posted.
        match unsafe { (*message).count } {
            0 => {}
            1 => {
                // SAFETY: as above, and the block belongs to the destination.
                unsafe {
                    (*message).count = 0;
                    dest.post_block((*message).blocks[0]);
                }
            }
            _ => {
                count!(self, MessagesPosted, 1);
                outbox.message.set(ptr::null_mut());
                // SAFETY: the message is a block of ours, given up here.
                unsafe { dest.post(message.cast(), message.cast()) };
            }
        }
    }

    /// Allocates a block for a message without looking at the inbox, which may be what is being
    /// read. Returns null if the region is out of room.
    fn alloc_message(&self, region: &Region<'_>) -> *mut Message {
        if let Some(block) = self.pop(MESSAGE_CLASS) {
            return block.cast();
        }
        if self.fill(region, MESSAGE_CLASS) == 0 {
            return ptr::null_mut();
        }
        self.pop(MESSAGE_CLASS).map_or(ptr::null_mut(), <*mut Block>::cast)
    }

    /// Reads the inbox if anything waits there and it is not being read already. Returns whether
    /// it read anything.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and must not be in the middle of returning blocks to one of
    /// this heap's slabs, which reading the inbox may also do.
    unsafe fn read_mail(&self, region: &Region<'_>) -> bool {
        if self.private.draining.get() || self.shared.inbox.load(Ordering::Relaxed).is_null() {
            return false;
        }
        self.private.draining.set(true);
        // SAFETY: forwarded.
        unsafe { self.drain_inbox(region) };
        self.private.draining.set(false);
        true
    }

    /// Takes everything in the inbox and files each block in its bin, or in its slab if the bin is
    /// full. Messages are then freed like any other block.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn drain_inbox(&self, region: &Region<'_>) {
        let mut node = self.shared.inbox.swap(ptr::null_mut(), Ordering::Acquire);
        if !node.is_null() {
            count!(self, MailReads, 1);
        }
        while !node.is_null() {
            let raw = node.map_addr(|addr| addr & !SINGLE);
            // SAFETY: inbox nodes are blocks given to us, linked through their first words, and the
            // acquire above makes the sender's writes visible.
            let next = unsafe { (*raw).next };
            if node.addr() & SINGLE != 0 {
                // SAFETY: a single node is an in-use block of one of our slabs.
                unsafe { self.file(region, raw) };
            } else {
                let message = raw.cast::<Message>();
                // SAFETY: as above, a message holds `count` blocks of our slabs, of `class` unless
                // it is mixed, and the blocks are read before the message is freed.
                unsafe {
                    let blocks = core::slice::from_raw_parts(
                        (&raw const (*message).blocks).cast::<*mut Block>(),
                        (*message).count as usize,
                    );
                    match (*message).class {
                        MIXED => blocks.iter().for_each(|&block| self.file(region, block)),
                        class => self.file_run(region, Class::from_index(class as usize), blocks),
                    }
                    self.free_small(message.cast());
                }
            }
            node = next;
        }
    }

    /// Takes in blocks of `class` of this heap's slabs that another thread sent back: as many as
    /// fit go into the bin with one copy, and the rest back to their slabs.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the blocks must be distinct in-use blocks of `class` of
    /// this heap's slabs, which are given up.
    unsafe fn file_run(&self, region: &Region<'_>, class: Class, blocks: &[*mut Block]) {
        count!(self, BlocksFiled, blocks.len());
        let bin = self.bin(class);
        let room = usize::from(bin.capacity.get()) - self.bin_len(class);
        let (kept, rest) = blocks.split_at(room.min(blocks.len()));
        let top = bin.top.get().wrapping_sub(kept.len());
        // SAFETY: the bin has room for `kept` blocks below its top.
        unsafe { ptr::copy_nonoverlapping(kept.as_ptr(), top, kept.len()) };
        bin.top.set(top);
        if !rest.is_empty() {
            // SAFETY: forwarded. The blocks are this heap's, so giving them back sends nothing.
            unsafe { self.give_back(region, class, rest, false) };
        }
    }

    /// Takes in a block of one of this heap's slabs that another thread sent back.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be an in-use block of one of this heap's
    /// slabs, which is given up.
    unsafe fn file(&self, region: &Region<'_>, block: *mut Block) {
        count!(self, BlocksFiled, 1);
        let entry = self.entry_of(block.cast());
        debug_assert_eq!(
            entry.owner(),
            self.private.id.get(),
            "a block was sent to the wrong heap"
        );
        if !self.push(entry.class(), block) {
            // SAFETY: forwarded.
            unsafe { self.free_to_slab(region, block, entry.class()) };
        }
    }

    /// Hands back everything this heap holds that is not in a slab: the inbox, the bins and the
    /// outboxes. Blocks of other heaps go to them on their own, so this allocates nothing.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn empty_caches(&self, region: &Region<'_>) {
        // SAFETY: forwarded.
        unsafe { self.drain_inbox(region) };
        // Before the bins, because returning a spill frees its batches, which are blocks too.
        for index in 0..CLASSES {
            // SAFETY: forwarded.
            unsafe { self.return_spill(region, Class::from_index(index), true) };
        }
        for kept in &self.private.kept {
            let block = kept.replace(Kept::NONE);
            if block.chunks != 0 {
                region.free_chunks(block.first, block.chunks);
            }
        }
        for outbox in &self.private.outboxes {
            // SAFETY: forwarded.
            unsafe { self.post_outbox(region, outbox) };
            let message = outbox.message.replace(ptr::null_mut());
            if !message.is_null() {
                // SAFETY: the message is an empty block that we hold.
                unsafe { self.give_back(region, MESSAGE_CLASS, &[message.cast()], true) };
            }
        }
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            let len = self.bin_len(class);
            let bin = &self.private.bins[index];
            let top = bin.top.get();
            bin.top.set(top.wrapping_add(len));
            // SAFETY: the bin held `len` blocks from `top`, which are in use and ours to give
            // back, and giving them back directly never touches a bin.
            unsafe { self.give_back(region, class, core::slice::from_raw_parts(top, len), true) };
            self.set_capacity(class, class.bin_initial());
            bin.misses.set(0);
        }
        self.private.grown.set(0);
    }

    /// Moves the oldest blocks of the full bin of `class`, up to half of it and at most a batch,
    /// into a spilled batch for the next refill of the class. A burst of frees bigger than the bin
    /// then costs a copy of pointers, where giving the blocks back costs a bitmap write per block
    /// now and a bitmap read per block when the class is refilled. Returns false, and moves
    /// nothing, if the class does not spill, its bin can still grow, the heap has spilled as much
    /// as it may, or there is no memory for a batch. In the third case the batches of the class go
    /// back to their slabs, because the burst outgrew them.
    fn spill(&self, region: &Region<'_>, class: Class) -> bool {
        let bin = self.bin(class);
        // Messages circulate between heaps, so they go home at once and are never held here.
        if class == BATCH_CLASS || class == MESSAGE_CLASS || bin.misses.get() & REFILLED == 0 {
            return false;
        }
        // Bursts that fit in a bin grown to its largest are the bin's to absorb, so a batch is
        // only for a bin that cannot grow any more.
        let capacity = usize::from(bin.capacity.get());
        let room = self.private.grown.get() + capacity * class.block_size() <= GROWTH_BUDGET;
        if capacity < class.bin_max() && room {
            return false;
        }
        let len = self.bin_len(class);
        let gone = (len - usize::from(bin.capacity.get()) / 2).min(BATCH_BLOCKS);
        let bytes = gone * class.block_size();
        if gone == 0 {
            return false;
        }
        if self.private.spilled.get() + bytes > SPILL_BUDGET {
            // A burst bigger than the budget is not one that the next refills take back. Its
            // oldest blocks, kept in batches, would hold on to a few blocks of every slab the burst
            // freed, which then could never be released, so they go back with the rest of it, and
            // the class spills again only after its next refill.
            bin.misses.set(bin.misses.get() & !REFILLED);
            // SAFETY: the caller holds the claim.
            unsafe { self.return_spill(region, class, false) };
            return false;
        }
        let batch = self.alloc_batch(region);
        if batch.is_null() {
            return false;
        }
        count!(self, BlocksSpilled, gone);
        let lists = self.lists(class);
        // The oldest blocks are at the end of the bin. Taking them out moves the rest down.
        let top = bin.top.get();
        // SAFETY: the bin holds `len` blocks from `top`, the batch is a fresh block of ours with
        // room for `gone` of them, and allocating it touched no bin but that of `BATCH_CLASS`.
        unsafe {
            ptr::copy_nonoverlapping(top.add(len - gone), (&raw mut (*batch).blocks).cast(), gone);
            ptr::copy(top, top.add(gone), len - gone);
            (*batch).count = gone;
            (*batch).next = lists.spill.get();
        }
        bin.top.set(top.wrapping_add(gone));
        lists.spill.set(batch);
        self.private.spilled.set(self.private.spilled.get() + bytes);
        true
    }

    /// Fills the empty bin of `class` from its newest spilled batch, and returns whether there was
    /// one. A batch bigger than the bin, which can happen after the bin shrank, keeps the rest.
    fn unspill(&self, region: &Region<'_>, class: Class) -> bool {
        let lists = self.lists(class);
        let batch = lists.spill.get();
        if batch.is_null() {
            return false;
        }
        let bin = self.bin(class);
        debug_assert_eq!(self.bin_len(class), 0, "a bin takes a batch only when it is empty");
        // SAFETY: spilled batches are blocks of ours that only this heap uses.
        let count = unsafe { (*batch).count };
        let take = count.min(usize::from(bin.capacity.get()));
        let first = bin.top.get().wrapping_sub(take);
        // SAFETY: the bin is empty, so the `take` slots below its end are free, and the batch
        // holds `count` blocks.
        unsafe {
            ptr::copy_nonoverlapping(
                (&raw const (*batch).blocks).cast::<*mut Block>().add(count - take),
                first,
                take,
            );
        }
        bin.top.set(first);
        count!(self, BlocksUnspilled, take);
        lists.unspilled.set(true);
        self.private.spilled.set(self.private.spilled.get() - take * class.block_size());
        if take == count {
            // SAFETY: as above.
            lists.spill.set(unsafe { (*batch).next });
            self.free_batch(region, batch);
        } else {
            // SAFETY: as above.
            unsafe { (*batch).count = count - take };
        }
        true
    }

    /// Gives every spilled batch of `class` back: its blocks go to their slabs or owners as
    /// [`give_back`](Self::give_back) sends them, `directly` or through the outboxes, and the
    /// batches themselves to their slab.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn return_spill(&self, region: &Region<'_>, class: Class, directly: bool) {
        let lists = self.lists(class);
        loop {
            let batch = lists.spill.get();
            if batch.is_null() {
                return;
            }
            // SAFETY: spilled batches are blocks of ours that only this heap uses, and each holds
            // `count` distinct in-use blocks of the class.
            let blocks = unsafe {
                lists.spill.set((*batch).next);
                core::slice::from_raw_parts(
                    (&raw const (*batch).blocks).cast::<*mut Block>(),
                    (*batch).count,
                )
            };
            self.private
                .spilled
                .set(self.private.spilled.get() - blocks.len() * class.block_size());
            #[cfg(feature = "metrics")]
            self.bin(BATCH_CLASS).frees.add(1);
            // SAFETY: the blocks are in use and ours to give back, and the batch is not read again.
            unsafe {
                self.give_back(region, class, blocks, directly);
                self.give_back(region, BATCH_CLASS, &[batch.cast()], true);
            }
        }
    }

    /// Allocates a block for a spilled batch without looking at the inbox or at spilled batches.
    /// Returns null if the region is out of room.
    fn alloc_batch(&self, region: &Region<'_>) -> *mut Batch {
        if let Some(block) = self.pop(BATCH_CLASS) {
            return block.cast();
        }
        if self.fill(region, BATCH_CLASS) == 0 {
            return ptr::null_mut();
        }
        self.pop(BATCH_CLASS).map_or(ptr::null_mut(), <*mut Block>::cast)
    }

    /// Frees a batch that a refill emptied, without spilling or reading the inbox.
    fn free_batch(&self, region: &Region<'_>, batch: *mut Batch) {
        #[cfg(feature = "metrics")]
        self.bin(BATCH_CLASS).frees.add(1);
        if !self.push(BATCH_CLASS, batch.cast()) {
            // SAFETY: a batch is an in-use block of one of this heap's slabs of `BATCH_CLASS`,
            // which is given up here.
            unsafe { self.free_to_slab(region, batch.cast(), BATCH_CLASS) };
        }
    }

    /// Handles a slab with no live blocks. Its chunks are returned unless it is the only active
    /// slab of its class, in which case it is kept for the next allocation.
    fn retire(&self, region: &Region<'_>, slab: Owned<'_>) {
        debug_assert_eq!(slab.used(), 0);
        let sole_active =
            slab.list() == List::Active && slab.prev().is_null() && slab.next().is_null();
        if !sole_active {
            self.release(region, slab);
        }
    }

    /// Returns an empty slab's chunks to the chunk allocator.
    fn release(&self, region: &Region<'_>, slab: Owned<'_>) {
        self.unlink(slab);
        count!(self, SlabsReleased, 1);
        #[cfg(feature = "metrics")]
        self.lists(slab.class()).slabs.sub(1);
        let first = region.chunk_index(slab.start());
        let chunks = slab.class().slab_chunks();
        if slab.was_filled() {
            self.private.held.set(self.private.held.get().saturating_sub(chunks));
            let filled = &self.lists(slab.class()).filled;
            filled.set(filled.get().saturating_sub(1));
        }
        region.free_chunks(first, chunks);
        region.purge_if_due();
        // Prefer the lowest free word of the home part next time, so slabs stay packed towards the
        // bottom of it.
        if region.home(self.slab_part(region, Class::from_index(0))).contains(&(first / 64)) {
            self.private.cursor.set(self.private.cursor.get().min(first / 64));
        }
    }

    /// Hands back everything this heap holds outside its slabs, then every empty slab.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    pub(crate) unsafe fn flush(&self, region: &Region<'_>) {
        // SAFETY: forwarded.
        unsafe { self.empty_caches(region) };
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            for list in [List::Active, List::Full] {
                let mut next = self.head(class, list).get();
                // SAFETY: slabs on our lists are ours, and `next` is read before the slab can move.
                while let Some(slab) = unsafe { next.as_ref() } {
                    // SAFETY: as above.
                    let slab = unsafe { slab.owned() };
                    next = slab.next();
                    if slab.used() == 0 {
                        self.release(region, slab);
                    }
                }
            }
        }
    }

    /// Allocates a run of `chunks` whole chunks, returning null if there is no room.
    pub(crate) fn alloc_huge(&self, region: &Region<'_>, chunks: usize) -> *mut u8 {
        self.alloc_huge_zeroed(region, chunks).0
    }

    /// As [`alloc_huge`](Self::alloc_huge), and also tells whether the block reads as zeros.
    fn alloc_huge_zeroed(&self, region: &Region<'_>, chunks: usize) -> (*mut u8, bool) {
        for kept in &self.private.kept {
            let block = kept.get();
            if block.chunks == chunks {
                kept.set(Kept::NONE);
                // The next kept block of this size has been asked for, so it stays a while.
                self.mark_kept_reused(chunks);
                count!(self, HugeCacheHits, 1);
                self.note_huge_alloc(chunks);
                return (region.chunk_ptr(block.first), false);
            }
        }
        // A block of whole chunks is a buffer or an arena that most programs touch all of, and
        // such blocks pack next to each other, so they use huge pages once the heap is big. One of
        // a huge page or more is worth it whatever the size of the heap.
        let huge = chunks >= HUGE_PAGE_CHUNKS || self.is_big();
        let part = region.part(self.private.node.get(), huge);
        let Some((first, zeroed)) = region.alloc_chunks_zeroed(chunks, 1, 0, part) else {
            return (ptr::null_mut(), false);
        };
        self.note_huge_alloc(chunks);
        region.set_huge(first, chunks);
        #[cfg(feature = "metrics")]
        if zeroed {
            count!(self, ZeroedFresh, 1);
        }
        (region.chunk_ptr(first), zeroed)
    }

    /// Records that a huge block of `chunks` chunks was handed out.
    fn note_huge_alloc(&self, chunks: usize) {
        self.private.runs.set(self.private.runs.get() + chunks);
        count!(self, HugeAllocs, 1);
        count!(self, HugeBytesAllocated, chunks << CHUNK_SHIFT);
    }

    /// Marks the kept huge blocks of `chunks` chunks as wanted.
    fn mark_kept_reused(&self, chunks: usize) {
        for kept in &self.private.kept {
            let block = kept.get();
            if block.chunks == chunks {
                kept.set(Kept { reused: true, ..block });
            }
        }
    }

    /// Moves a slab to the full list, or to the back of the active list.
    ///
    /// A slab that becomes active again usually has a single free block. Refills keep using the
    /// slab at the front until it runs dry, and by the time the returning slab gets its turn it has
    /// collected many more blocks. At the front it would be drained and parked again after nearly
    /// every free.
    fn move_to(&self, slab: Owned<'_>, list: List) {
        self.unlink(slab);
        match list {
            List::Active => self.push_back(slab),
            List::Full | List::None => self.push_front(slab, list),
        }
    }

    fn push_front(&self, slab: Owned<'_>, list: List) {
        let head = self.head(slab.class(), list);
        let old = head.get();
        slab.set_prev(ptr::null());
        slab.set_next(old);
        // SAFETY: slabs on our lists are ours.
        if let Some(old) = unsafe { old.as_ref() } {
            // SAFETY: as above.
            unsafe { old.owned() }.set_prev(slab.slab());
        }
        head.set(slab.slab());
        if list == List::Active && old.is_null() {
            self.lists(slab.class()).active_last.set(slab.slab());
        }
        slab.set_list(list);
    }

    fn push_back(&self, slab: Owned<'_>) {
        let lists = self.lists(slab.class());
        let last = lists.active_last.get();
        // SAFETY: slabs on our lists are ours.
        let Some(last) = (unsafe { last.as_ref() }) else {
            return self.push_front(slab, List::Active);
        };
        slab.set_prev(ptr::from_ref(last));
        slab.set_next(ptr::null());
        // SAFETY: as above.
        unsafe { last.owned() }.set_next(slab.slab());
        lists.active_last.set(slab.slab());
        slab.set_list(List::Active);
    }

    fn unlink(&self, slab: Owned<'_>) {
        let list = slab.list();
        if list == List::None {
            return;
        }
        let (prev, next) = (slab.prev(), slab.next());
        // SAFETY: neighbours on our lists are ours.
        match unsafe { prev.as_ref() } {
            // SAFETY: as above.
            Some(prev) => unsafe { prev.owned() }.set_next(next),
            None => self.head(slab.class(), list).set(next),
        }
        // SAFETY: as above.
        if let Some(next) = unsafe { next.as_ref() } {
            // SAFETY: as above.
            unsafe { next.owned() }.set_prev(prev);
        } else if list == List::Active {
            self.lists(slab.class()).active_last.set(prev);
        }
        slab.set_prev(ptr::null());
        slab.set_next(ptr::null());
        slab.set_list(List::None);
    }

    /// Checks every structural invariant of this heap and panics on a violation. Every block it
    /// finds outside a slab goes into `seen`, which must not hold it already.
    ///
    /// It walks all lists, bins, outboxes and the inbox, so it is for tests. Each slab's counter
    /// must match its free list, each slab must be on the list its occupancy calls for, every
    /// block in a bin must be of the bin's class and may belong to another heap only if the class
    /// allows that, and every block in an outbox or in the inbox must belong to the heap it is for.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot and must not run it concurrently with remote
    /// frees, which would change the inbox under the walk.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) unsafe fn validate(&self, seen: &mut std::collections::BTreeSet<usize>) {
        // SAFETY: the caller holds the claim.
        let region = unsafe { self.region() };
        let id = self.private.id.get();
        let mut claim = |block: *mut Block, what: &str| {
            assert!(seen.insert(block.addr()), "block {block:p} appears twice, last in {what}");
        };
        let check = |block: *mut Block, what: &str| -> Entry {
            assert!(region.contains(block.cast()), "{what} holds a block outside the region");
            let entry = self.entry_of(block.cast());
            let class = entry.class();
            let first = region.chunk_index(block.cast()) & !(class.slab_chunks() - 1);
            let offset = block.addr() - region.chunk_ptr(first).addr();
            assert!(
                offset.is_multiple_of(class.block_size()) &&
                    offset / class.block_size() < class.blocks_per_slab(),
                "{what} holds a pointer that is not a block of its slab"
            );
            if entry.owner() == id {
                // SAFETY: the slab is ours.
                let slab =
                    unsafe { region.slab_of(NonNull::new_unchecked(block.cast()), class).owned() };
                assert!(
                    !slab.is_free(block.cast()),
                    "{what} holds a block its slab counts as free"
                );
            }
            entry
        };
        let mut spilled = 0;
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            let lists = &self.private.classes[index];
            for (list, head) in [(List::Active, lists.active.get()), (List::Full, lists.full.get())]
            {
                let mut prev: *const Slab = ptr::null();
                let mut current = head;
                // SAFETY: slabs on our lists are ours.
                while let Some(slab) = unsafe { current.as_ref() } {
                    // SAFETY: as above.
                    let owned = unsafe { slab.owned() };
                    assert_eq!(owned.list(), list, "slab is on the wrong list");
                    assert_eq!(owned.class(), class, "slab is on the wrong class");
                    assert!(ptr::eq(owned.prev(), prev), "broken back link");
                    let entry = self.entry_of(owned.start());
                    assert_eq!(entry, Entry::new(id, class), "the chunk map disagrees with a slab");
                    assert_eq!(
                        owned.is_exhausted(),
                        list == List::Full,
                        "a slab with free blocks must be active and an exhausted one full"
                    );
                    owned.check();
                    prev = current;
                    current = owned.next();
                }
                if list == List::Active {
                    assert!(ptr::eq(lists.active_last.get(), prev), "wrong last active slab");
                }
            }
            let bin = &self.private.bins[index];
            assert!(
                self.bin_len(class) <= usize::from(bin.capacity.get()),
                "a bin is over capacity"
            );
            let mut slot = bin.top.get();
            for _ in 0..self.bin_len(class) {
                // SAFETY: slots from the top to the end of a bin hold blocks.
                let block = unsafe { *slot };
                let entry = check(block, "a bin");
                assert_eq!(entry.class(), class, "a bin holds a block of another class");
                assert!(
                    entry.owner() == id || bin.anywhere.get(),
                    "a bin holds another heap's block of a class that must go home"
                );
                claim(block, "a bin");
                slot = slot.wrapping_add(1);
            }
            let mut batch = lists.spill.get();
            while !batch.is_null() {
                assert_eq!(check(batch.cast(), "a spill").class(), BATCH_CLASS);
                claim(batch.cast(), "a spill");
                // SAFETY: spilled batches are blocks of ours, each holding `count` blocks.
                let blocks = unsafe { &(&(*batch).blocks)[..(*batch).count] };
                assert!(!blocks.is_empty() && class != BATCH_CLASS, "a spilled batch is wrong");
                for &block in blocks {
                    let entry = check(block, "a spill");
                    assert_eq!(entry.class(), class, "a spill holds a block of another class");
                    assert!(
                        entry.owner() == id || bin.anywhere.get(),
                        "a spill holds another heap's block of a class that must go home"
                    );
                    claim(block, "a spill");
                    spilled += class.block_size();
                }
                // SAFETY: as above.
                batch = unsafe { (*batch).next };
            }
        }
        assert_eq!(spilled, self.private.spilled.get(), "the spill budget disagrees");
        for outbox in &self.private.outboxes {
            let message = outbox.message.get();
            if message.is_null() {
                continue;
            }
            assert_eq!(check(message.cast(), "an outbox").class(), MESSAGE_CLASS);
            claim(message.cast(), "an outbox");
            // SAFETY: the message is ours until it is posted.
            let blocks = unsafe { &(&(*message).blocks)[..(*message).count as usize] };
            assert!(blocks.len() < MESSAGE_BLOCKS, "a full message was not posted");
            // SAFETY: as above.
            let class = unsafe { (*message).class };
            assert!(
                blocks.is_empty() ||
                    class == MIXED ||
                    blocks.iter().all(
                        |&block| self.entry_of(block.cast()).class().index() == class as usize
                    ),
                "a message of one class holds a block of another"
            );
            for &block in blocks {
                let entry = check(block, "an outbox");
                assert_eq!(
                    entry.owner(),
                    outbox.dest.get(),
                    "an outbox holds a block for another heap"
                );
                assert_ne!(entry.owner(), id, "an outbox holds a block of this heap");
                claim(block, "an outbox");
            }
        }
        let mut node = self.shared.inbox.load(Ordering::Acquire);
        while !node.is_null() {
            let raw = node.map_addr(|addr| addr & !SINGLE);
            if node.addr() & SINGLE != 0 {
                assert_eq!(
                    check(raw, "the inbox").owner(),
                    id,
                    "a block was sent to the wrong heap"
                );
                claim(raw, "the inbox");
            } else {
                let message = raw.cast::<Message>();
                claim(raw, "the inbox");
                // SAFETY: nothing frees remotely while this runs, so the inbox is stable.
                for &block in unsafe { &(&(*message).blocks)[..(*message).count as usize] } {
                    assert_eq!(check(block, "the inbox").owner(), id, "a message went astray");
                    claim(block, "the inbox");
                }
            }
            // SAFETY: as above.
            node = unsafe { (*raw).next };
        }
    }
}

/// A claimed heap, which is one thread's handle for allocating from a [`Region`].
///
/// A heap is the unit of single-threaded access. It is `Send`, so it can move between threads, but
/// it is not `Sync`. Dropping it releases its slot and its slabs to the next claimant. Get one from
/// [`Region::heap`].
///
/// Blocks are not tied to the heap that allocated them. Any heap, or the region itself through
/// [`Region::dealloc`], can free them.
/// ```
/// use bcmalloc::{Config, Mapping};
/// use core::alloc::Layout;
///
/// let mapping = Mapping::new(&Config::new().capacity(64 << 20).max_heaps(4))?;
/// let region = mapping.region();
/// let heap = region.heap().expect("a free heap");
///
/// let layout = Layout::new::<[u64; 8]>();
/// let block = heap.alloc(layout).expect("memory");
/// // SAFETY: the block came from this region with this layout and is not used again.
/// unsafe { heap.dealloc(block, layout) };
/// # Ok::<(), bcmalloc::Error>(())
/// ```
#[derive(Debug)]
pub struct Heap<'r> {
    region: &'r Region<'r>,
    data: &'r HeapData,
    /// Heaps are used from one thread at a time.
    _not_sync: PhantomData<Cell<()>>,
}

// SAFETY: a claimed heap has no thread affinity, and moving it moves the exclusive claim with it.
unsafe impl Send for Heap<'_> {}

impl<'r> Heap<'r> {
    /// Wraps a slot that the caller has just claimed.
    pub(crate) const fn new(region: &'r Region<'r>, data: &'r HeapData) -> Self {
        Self { region, data, _not_sync: PhantomData }
    }

    /// The region this heap allocates from.
    pub const fn region(&self) -> &'r Region<'r> {
        self.region
    }

    /// Allocates memory for `layout`, or returns `None` if the region has no room or the layout
    /// is not supported (alignment above [`MAX_ALIGN`](crate::MAX_ALIGN)).
    ///
    /// The memory is uninitialised. Zero-sized layouts are served like one-byte ones.
    #[inline]
    pub fn alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: this handle holds the claim.
        NonNull::new(unsafe { self.data.alloc(layout) })
    }

    /// As [`alloc`](Self::alloc), but the memory is zeroed.
    pub fn alloc_zeroed(&self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: this handle holds the claim.
        NonNull::new(unsafe { self.data.alloc_zeroed(layout) })
    }

    /// Frees a block.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by [`alloc`](Self::alloc),
    /// [`alloc_zeroed`](Self::alloc_zeroed) or [`realloc`](Self::realloc) on this region with
    /// exactly `layout`, and must not be used or freed again.
    #[inline]
    pub unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: this handle holds the claim, and the caller vouches for the block.
        unsafe { self.data.dealloc(ptr, layout) };
    }

    /// Frees a block without its layout, and ignores a pointer outside the region.
    ///
    /// # Safety
    ///
    /// A pointer inside the region must be a live block of it, and must not be used or freed
    /// again.
    pub unsafe fn free_unsized(&self, ptr: NonNull<u8>) {
        // SAFETY: this handle holds the claim, and the caller vouches for the block.
        unsafe { self.data.free_unsized(ptr) };
    }

    /// Resizes a block, returning `None` (and leaving the block untouched) if there is no room.
    ///
    /// A block of a slab stays where it is while the new size fits in it and fills at least half
    /// of it. A block of whole chunks that shrinks gives back the chunks past its new end, and one
    /// that grows takes the chunks right after it if they are free. Otherwise the contents are
    /// copied to a new block.
    ///
    /// # Safety
    ///
    /// `ptr` must be a live block of this region allocated with exactly `old`, and `new_size` must
    /// be non-zero and not overflow when rounded up to `old.align()`.
    pub unsafe fn realloc(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: this handle holds the claim, and the caller vouches for the block and size.
        NonNull::new(unsafe { self.data.realloc(ptr, old, new_size) })
    }

    /// Makes this heap take its chunks from the part of the region of NUMA node `node`, as if its
    /// thread had started there.
    #[cfg(any(test, feature = "testing"))]
    pub fn settle_on(&self, node: usize) {
        self.data.settle_on(self.region, node);
    }

    /// Checks the heap's structural invariants. See `HeapData::validate`.
    #[cfg(any(test, feature = "testing"))]
    pub fn validate(&self) {
        self.validate_into(&mut std::collections::BTreeSet::new());
    }

    /// As [`validate`](Self::validate), adding every block found outside a slab to `seen`, which
    /// must not hold it already. Checking several heaps into one set finds a block held twice.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn validate_into(&self, seen: &mut std::collections::BTreeSet<usize>) {
        // SAFETY: this handle holds the claim. Tests do not free remotely while it runs.
        unsafe { self.data.validate(seen) };
    }

    /// Gives up this handle without releasing the claim, which the caller takes over. The slot
    /// stays claimed until [`release_claim`](HeapData::release_claim) is called on it.
    #[cfg(all(feature = "std", not(loom)))]
    pub(crate) const fn leak(self) -> &'r HeapData {
        let data = self.data;
        core::mem::forget(self);
        data
    }

    /// Hands back every block this heap holds outside its slabs, then every empty slab, so that
    /// other heaps (and large allocations) can use the memory. [`Region::trim`] does this for every
    /// idle heap.
    pub fn flush(&self) {
        // SAFETY: this handle holds the claim.
        unsafe { self.data.flush(self.region) };
    }
}

impl Backend for Heap<'_> {
    unsafe fn allocate_raw(&self, layout: Layout) -> *mut u8 {
        self.alloc(layout).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn allocate_zeroed_raw(&self, layout: Layout) -> *mut u8 {
        self.alloc_zeroed(layout).map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn deallocate_raw(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded.
        unsafe { self.dealloc(ptr, layout) };
    }

    unsafe fn resize_raw(&self, ptr: NonNull<u8>, old: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded.
        unsafe { self.realloc(ptr, old, new_size) }.map_or(ptr::null_mut(), NonNull::as_ptr)
    }
}

allocator!(Allocator for Heap<'_>);

impl Drop for Heap<'_> {
    fn drop(&mut self) {
        // SAFETY: this handle holds the claim.
        unsafe { self.data.give_up() };
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;
    use crate::{
        chunks::CHUNK_SIZE,
        testing::{Harness, Rng, TestRegion, iterations, random_traffic},
    };
    use core::{
        alloc::Layout,
        mem::{MaybeUninit, align_of, offset_of, size_of},
        ptr::NonNull,
    };
    use std::vec::Vec;

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// How much of the region is in use, in bytes.
    fn used(region: &Region<'_>) -> usize {
        let stats = region.stats();
        stats.capacity - stats.free
    }

    fn alloc_many(heap: &Heap<'_>, layout: Layout, count: usize) -> Vec<NonNull<u8>> {
        (0..count).map(|_| heap.alloc(layout).expect("region has room")).collect()
    }

    /// Frees blocks through `heap`.
    fn free_all(heap: &Heap<'_>, layout: Layout, blocks: Vec<NonNull<u8>>) {
        for block in blocks {
            // SAFETY: the blocks were allocated with this layout and are freed once.
            unsafe { heap.dealloc(block, layout) };
        }
    }

    /// Whether anything waits in the inbox of `heap`.
    fn has_mail(heap: &Heap<'_>) -> bool {
        !heap.data.shared.inbox.load(Ordering::Relaxed).is_null()
    }

    /// A size whose class shares cache lines between blocks, so its blocks always go home.
    const HOMEBOUND: usize = 100;

    /// A size whose class never shares a line, so any heap may reuse its blocks.
    const ANYWHERE: usize = 256;

    #[test]
    fn the_test_sizes_fall_on_both_sides_of_the_reuse_rule() {
        assert!(!Class::for_size(HOMEBOUND).reusable_anywhere());
        assert!(Class::for_size(ANYWHERE).reusable_anywhere());
        assert!(reusable_anywhere(MESSAGE_CLASS), "messages are recycled by whoever reads them");
    }

    #[test]
    fn a_heap_slot_keeps_what_other_threads_write_on_its_own_line() {
        let line = align_of::<CacheLine>();
        assert_eq!(align_of::<HeapData>(), line);
        assert_eq!(offset_of!(HeapData, shared) % line, 0);
        assert_eq!(offset_of!(HeapData, private) % line, 0);
        assert_ne!(offset_of!(HeapData, shared), offset_of!(HeapData, private));
        assert_eq!(size_of::<HeapData>() % line, 0);
    }

    #[test]
    fn blocks_are_distinct_aligned_and_keep_their_contents() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let mut harness = Harness::new(&region, 1);
        let mut rng = Rng::new(1);
        for _ in 0..iterations(2_000) {
            harness.alloc(0, rng.between(1, 3000), 1 << rng.below(8));
        }
        harness.finish();
    }

    #[test]
    fn a_freed_block_is_the_next_one_handed_out() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        for size in [16, HOMEBOUND, ANYWHERE, 5000, 100_000] {
            let layout = layout(size, 8);
            let block = heap.alloc(layout).unwrap();
            // SAFETY: allocated just above with this layout.
            unsafe { heap.dealloc(block, layout) };
            assert_eq!(heap.alloc(layout), Some(block), "{size}");
        }
    }

    #[test]
    fn allocation_and_free_in_a_loop_never_grows_the_region() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(200, 8);
        let block = heap.alloc(layout).unwrap();
        // SAFETY: allocated just above with this layout.
        unsafe { heap.dealloc(block, layout) };
        let baseline = used(&region);
        for _ in 0..iterations(10_000) {
            let block = heap.alloc(layout).unwrap();
            // SAFETY: as above.
            unsafe { heap.dealloc(block, layout) };
        }
        assert_eq!(used(&region), baseline);
    }

    #[test]
    fn empty_slabs_go_back_except_one_and_flush_returns_the_rest() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(64, 8);
        // 64-byte blocks are 1024 to a slab, so this spans three slabs.
        let blocks = alloc_many(&heap, layout, 3000);
        assert_eq!(used(&region), 3 * CHUNK_SIZE);

        free_all(&heap, layout, blocks);
        // The bin keeps a few blocks, which may hold a second slab, and one empty slab is kept.
        assert!(used(&region) <= 2 * CHUNK_SIZE, "empty slabs should be released");
        heap.validate();
        heap.flush();
        assert_eq!(used(&region), 0);
    }

    #[test]
    fn a_burst_bigger_than_the_bin_spills_and_the_next_burst_takes_it_back() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(64, 8);
        let burst = 4 * Class::for_size(64).bin_max();
        // Bursts that alternate with refills grow the bin to its largest, and only then does an
        // overflow spill.
        for _ in 0..8 {
            let blocks = alloc_many(&heap, layout, burst);
            free_all(&heap, layout, blocks);
        }
        heap.validate();
        assert!(heap.data.private.spilled.get() > 0, "an overflowing bin spills");
        let slabs = used(&region);
        // The next burst takes the spilled blocks back before it touches a slab.
        let blocks = alloc_many(&heap, layout, burst);
        assert_eq!(used(&region), slabs, "the next burst reuses the last one's blocks");
        heap.validate();
        free_all(&heap, layout, blocks);
        heap.flush();
        assert_eq!(heap.data.private.spilled.get(), 0);
        assert_eq!(used(&region), 0, "a flush returns spilled blocks to their slabs");
    }

    #[test]
    fn a_burst_bigger_than_the_spill_budget_keeps_no_batches() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(64, 8);
        let class = Class::for_size(64);
        for _ in 0..8 {
            let blocks = alloc_many(&heap, layout, 4 * class.bin_max());
            free_all(&heap, layout, blocks);
        }
        assert!(!heap.data.lists(class).spill.get().is_null(), "bursts that fit spill");
        // Twice the budget: batches would pin a few blocks of every slab the burst freed.
        let burst = 2 * SPILL_BUDGET / class.block_size();
        let blocks = alloc_many(&heap, layout, burst);
        free_all(&heap, layout, blocks);
        heap.validate();
        assert!(heap.data.lists(class).spill.get().is_null(), "the burst outgrew the batches");
        assert_eq!(heap.data.private.spilled.get(), 0);
        heap.flush();
        assert_eq!(used(&region), 0);
    }

    #[test]
    fn spilled_batches_that_no_refill_takes_go_back_at_maintenance() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let (layout, other) = (layout(64, 8), layout(1024, 8));
        let class = Class::for_size(64);
        for _ in 0..8 {
            let blocks = alloc_many(&heap, layout, 4 * class.bin_max());
            free_all(&heap, layout, blocks);
        }
        assert!(!heap.data.lists(class).spill.get().is_null());
        // Refills of another class run maintenance steps, which return the idle class's batches.
        for _ in 0..4 {
            let blocks = alloc_many(&heap, other, 16 * MAINTENANCE_INTERVAL as usize);
            free_all(&heap, other, blocks);
        }
        heap.validate();
        assert!(heap.data.lists(class).spill.get().is_null(), "idle batches go back");
    }

    #[test]
    fn a_heap_that_only_frees_a_class_never_spills_it() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, freer) = (region.heap().unwrap(), region.heap().unwrap());
        // 64-byte blocks may stay in the freeing heap's bin, but that heap never allocates them.
        let layout = layout(64, 8);
        for _ in 0..8 {
            let blocks = alloc_many(&owner, layout, 4 * Class::for_size(64).bin_max());
            free_all(&freer, layout, blocks);
        }
        freer.validate();
        assert_eq!(freer.data.private.spilled.get(), 0, "no refill would take a spill");
        freer.flush();
        owner.flush();
    }

    #[test]
    fn a_full_bin_gives_back_its_older_half() {
        let test = TestRegion::new(16, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let class = Class::for_size(ANYWHERE);
        let layout = layout(ANYWHERE, 8);
        let blocks = alloc_many(&heap, layout, 4 * class.bin_max());
        let newest = *blocks.last().unwrap();
        free_all(&heap, layout, blocks);
        let capacity = usize::from(heap.data.private.bins[class.index()].capacity.get());
        assert!(capacity > class.bin_initial(), "a bin that runs dry and overflows grows");
        let len = heap.data.bin_len(class);
        assert!(len > capacity / 2 && len <= capacity, "{len} blocks are left in the bin");
        assert_eq!(heap.alloc(layout), Some(newest), "the newest blocks stay");
        heap.validate();
    }

    #[test]
    fn huge_allocations_span_chunks_and_release_them() {
        let test = TestRegion::new(160, 1);
        let region = test.region();
        let mut harness = Harness::new(&region, 1);
        // The first spans nine chunks inside one bitmap word, the second needs two words.
        assert!(harness.alloc(0, 576 << 10, 1));
        assert!(harness.alloc(0, 5 << 20, 4096));
        assert_eq!(used(&region), (9 + 80) * CHUNK_SIZE);
        harness.finish();
    }

    #[test]
    fn blocks_freed_by_another_heap_go_home_and_are_reused_by_the_owner() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        let layout = layout(HOMEBOUND, 8);
        let blocks = alloc_many(&owner, layout, 500);

        free_all(&other, layout, blocks);
        other.flush();
        owner.validate();
        other.validate();
        let baseline = used(&region);
        let again = alloc_many(&owner, layout, 500);
        assert_eq!(used(&region), baseline, "the owner must recycle what the other heap freed");
        free_all(&owner, layout, again);
    }

    #[test]
    fn a_heap_that_only_frees_takes_back_its_messages() {
        // Found by mimalloc-bench's xmalloc-test. A consumer sends blocks home in messages of its
        // own, which come back to it once read. A consumer that never allocates used to leave them
        // in its inbox and keep allocating new ones.
        let test = TestRegion::new(256, 2);
        let region = test.region();
        let (producer, consumer) = (region.heap().unwrap(), region.heap().unwrap());
        let layout = layout(HOMEBOUND, 8);
        for _ in 0..iterations(400) {
            let blocks = alloc_many(&producer, layout, 2000);
            free_all(&consumer, layout, blocks);
        }
        producer.validate();
        consumer.validate();
        assert!(used(&region) < 32 * CHUNK_SIZE, "{} chunks in use", used(&region) / CHUNK_SIZE);
    }

    #[test]
    fn a_heap_reuses_blocks_of_others_only_where_they_share_no_line() {
        let test = TestRegion::new(16, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        for (size, reused) in [(ANYWHERE, true), (HOMEBOUND, false)] {
            let layout = layout(size, 8);
            let block = owner.alloc(layout).unwrap();
            // SAFETY: allocated by the owner with this layout and freed once.
            unsafe { other.dealloc(block, layout) };
            let next = other.alloc(layout).unwrap();
            assert_eq!(next == block, reused, "{size}");
            owner.validate();
            other.validate();
        }
    }

    #[test]
    fn an_outbox_is_posted_when_its_message_fills_or_on_flush() {
        let test = TestRegion::new(16, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        let layout = layout(HOMEBOUND, 8);
        let blocks = alloc_many(&owner, layout, MESSAGE_BLOCKS + 1);
        let mut blocks = blocks.into_iter();
        free_all(&other, layout, blocks.by_ref().take(MESSAGE_BLOCKS - 1).collect());
        assert!(!has_mail(&owner), "a message is held back until it fills");
        free_all(&other, layout, blocks.by_ref().take(1).collect());
        assert!(has_mail(&owner), "a full message is posted");
        owner.validate();
        other.validate();

        free_all(&other, layout, blocks.collect());
        other.flush();
        let node = owner.data.shared.inbox.load(Ordering::Relaxed);
        assert_eq!(node.addr() & SINGLE, SINGLE, "one block is sent on its own");
        // The owner frees the messages, which belong to the other heap, and that heap takes them
        // back when it flushes.
        owner.flush();
        other.flush();
        assert_eq!(used(&region), 0, "everything came home and was released");
    }

    #[test]
    fn a_message_for_another_heap_posts_what_the_outbox_holds() {
        let test = TestRegion::new(16, 3);
        let region = test.region();
        let heaps = [region.heap().unwrap(), region.heap().unwrap(), region.heap().unwrap()];
        // Ids 0 and 16 would share an outbox, but there are only three heaps, so make the third
        // heap's frees land in the outbox the first heap's use by freeing for two owners in turn.
        let layout = layout(HOMEBOUND, 8);
        let a = heaps[0].alloc(layout).unwrap();
        let b = heaps[1].alloc(layout).unwrap();
        // SAFETY: allocated with this layout and freed once.
        unsafe {
            heaps[2].dealloc(a, layout);
            heaps[2].dealloc(b, layout);
        }
        assert!(!has_mail(&heaps[0]) && !has_mail(&heaps[1]), "different outboxes hold them");
        for heap in &heaps {
            heap.validate();
        }
        heaps[2].flush();
        assert!(has_mail(&heaps[0]) && has_mail(&heaps[1]));
    }

    #[test]
    fn formatting_a_region_does_not_read_state_owned_by_a_heap() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                heap.alloc(layout(8, 8)).unwrap();
            });
            // Miri flags this if formatting reads anything that only the heap's claimant may read.
            let _ = std::format!("{region:?}");
        });
    }

    #[test]
    #[should_panic(expected = "at least one chunk")]
    fn a_test_region_needs_a_chunk() {
        let _ = TestRegion::new(0, 1);
    }

    #[test]
    fn an_exhausted_region_fails_cleanly_and_recovers() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(8192, 8);
        let mut blocks = Vec::new();
        while let Some(block) = heap.alloc(layout) {
            blocks.push(block);
        }
        // Eight one-chunk slabs of eight blocks each.
        assert_eq!(blocks.len(), 64);
        assert_eq!(region.stats().free, 0);
        assert!(heap.alloc(layout).is_none());

        free_all(&heap, layout, blocks.drain(..8).collect());
        assert!(heap.alloc(layout).is_some());
    }

    #[test]
    fn a_failed_allocation_reclaims_this_heaps_empty_slabs_and_retries() {
        // Nine chunks, so one small slab leaves eight, which cannot hold a nine-chunk request.
        let test = TestRegion::new(9, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let small = layout(16, 8);
        let block = heap.alloc(small).unwrap();
        // SAFETY: allocated above with this layout.
        unsafe { heap.dealloc(block, small) };
        assert_eq!(used(&region), CHUNK_SIZE, "the empty slab is retained");

        let huge = layout(9 * CHUNK_SIZE, 1);
        assert!(heap.alloc(huge).is_some(), "retained slabs must not block a large allocation");
    }

    #[test]
    fn realloc_stays_in_place_within_a_class_and_copies_across_classes() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let old = layout(100, 8);
        let block = heap.alloc(old).unwrap();
        // SAFETY: the block holds 100 bytes.
        unsafe { block.as_ptr().write_bytes(0xAB, 100) };

        // SAFETY: the block is live with the layout given.
        let same = unsafe { heap.realloc(block, old, 104) }.unwrap();
        assert_eq!(same, block, "100 and 104 bytes share a size class");

        // SAFETY: the block is still live, now with the 104-byte layout.
        let grown = unsafe { heap.realloc(same, layout(104, 8), 5000) }.unwrap();
        assert_ne!(grown, block);
        // SAFETY: the grown block holds at least 100 initialised bytes.
        let copied = unsafe { core::slice::from_raw_parts(grown.as_ptr(), 100) };
        assert!(copied.iter().all(|&byte| byte == 0xAB));
        // SAFETY: allocated by the call above.
        unsafe { heap.dealloc(grown, layout(5000, 8)) };
    }

    #[test]
    fn a_block_that_shrinks_by_at_most_half_keeps_its_place_and_its_class() {
        let test = TestRegion::new(16, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let block = heap.alloc(layout(1000, 8)).unwrap();
        // SAFETY: the block is live with this layout.
        let same = unsafe { heap.realloc(block, layout(1000, 8), 600) }.unwrap();
        assert_eq!(same, block, "600 bytes waste less than half of a 1 KiB block");
        // SAFETY: as above, now with 600 bytes.
        let moved = unsafe { heap.realloc(same, layout(600, 8), 300) }.unwrap();
        assert_ne!(moved, block, "300 bytes would waste more than half");
        // The kept block was freed with a 600-byte layout and still went back as a 1 KiB block.
        assert_eq!(heap.alloc(layout(1024, 8)), Some(block));
        heap.validate();
    }

    #[test]
    fn a_freed_huge_block_is_kept_for_the_next_request_of_its_size() {
        let test = TestRegion::new(320, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let big = layout(1 << 20, 8);
        let block = heap.alloc(big).unwrap();
        // SAFETY: allocated just above with this layout.
        unsafe { heap.dealloc(block, big) };
        assert_eq!(used(&region), 16 * CHUNK_SIZE, "the block is kept, still claimed");
        assert_eq!(heap.alloc(big), Some(block), "and handed out again");
        // SAFETY: as above.
        unsafe { heap.dealloc(block, big) };
        let other = layout(2 << 20, 8);
        let second = heap.alloc(other).unwrap();
        assert_ne!(second, block, "a request of another size does not take it");
        // SAFETY: as above.
        unsafe { heap.dealloc(second, other) };
        let giant = layout(12 << 20, 8);
        let third = heap.alloc(giant).unwrap();
        // SAFETY: as above.
        unsafe { heap.dealloc(third, giant) };
        assert_eq!(used(&region), 48 * CHUNK_SIZE, "a block over 8 MiB is not kept");
        heap.flush();
        assert_eq!(used(&region), 0, "a flush gives kept blocks back");
    }

    #[test]
    fn huge_blocks_shrink_in_place_and_grow_into_free_chunks() {
        let test = TestRegion::new(64, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let big = layout(20 * CHUNK_SIZE, 8);
        let block = heap.alloc(big).unwrap();
        assert_eq!(used(&region), 20 * CHUNK_SIZE);
        // SAFETY: the block is live with this layout.
        let shrunk = unsafe { heap.realloc(block, big, 12 * CHUNK_SIZE) }.unwrap();
        assert_eq!((shrunk, used(&region)), (block, 12 * CHUNK_SIZE), "the tail goes back");
        // SAFETY: as above, now with twelve chunks.
        let grown = unsafe { heap.realloc(shrunk, layout(12 * CHUNK_SIZE, 8), 18 * CHUNK_SIZE) };
        assert_eq!(grown, Some(block), "the chunks after the block were free");
        assert_eq!(used(&region), 18 * CHUNK_SIZE);
        // SAFETY: as above, now with eighteen chunks.
        unsafe { heap.dealloc(block, layout(18 * CHUNK_SIZE, 8)) };
        heap.flush();
        assert_eq!(used(&region), 0);
    }

    #[test]
    fn heaps_take_chunks_from_their_own_node_first() {
        // Four bitmap words, two for each node.
        let test = TestRegion::with_nodes(256, 2, 2);
        let region = test.region();
        let (near, far) = (region.heap().unwrap(), region.heap().unwrap());
        near.settle_on(0);
        far.settle_on(1);
        let small = layout(8192, 8);
        let chunk = |block: NonNull<u8>| region.chunk_index(block.as_ptr());
        let mine = alloc_many(&near, small, 8 * 100);
        assert!(mine.iter().all(|&block| chunk(block) < 128), "node 0 owns chunks 0 to 127");
        let theirs = alloc_many(&far, small, 8 * 100);
        assert!(theirs.iter().all(|&block| chunk(block) >= 128), "node 1 owns chunks 128 up");
        // Node 0's part has 28 chunks left, so a request for 40 more falls back to node 1's part.
        let more = alloc_many(&near, small, 8 * 40);
        assert!(more.iter().any(|&block| chunk(block) >= 128), "a full part borrows from others");
        let huge = far.alloc(layout(9 * CHUNK_SIZE, 8)).unwrap();
        assert!(chunk(huge) >= 128, "huge blocks come from the node's part too");
        near.validate();
        far.validate();
    }
    #[test]
    fn frees_without_a_layout_find_small_and_huge_blocks_and_skip_others() {
        let test = TestRegion::new(256, 2);
        let region = test.region();
        let (heap, other) = (region.heap().unwrap(), region.heap().unwrap());
        let small = alloc_many(&heap, layout(48, 8), 100);
        let theirs = alloc_many(&other, layout(4000, 8), 10);
        let huge = heap.alloc(layout(9 * CHUNK_SIZE + 1, 8)).unwrap();
        let mut outside = 0_u64;
        // SAFETY: each block is live and freed once, and the last pointer is not in the region.
        unsafe {
            for &block in small.iter().chain(&theirs) {
                heap.free_unsized(block);
            }
            heap.free_unsized(huge);
            heap.free_unsized(NonNull::from(&mut outside).cast());
        }
        heap.flush();
        other.flush();
        // The other heap frees the messages it was sent, which are this heap's blocks.
        heap.flush();
        heap.validate();
        other.validate();
        assert_eq!(used(&region), 0, "every block went back to its slab or the chunk allocator");
    }
    #[test]
    fn shrinking_a_bin_that_takes_in_mail_while_it_shrinks_keeps_it_within_capacity() {
        // Found by mimalloc-bench's mstress. Shrinking an idle bin gives its oldest blocks back.
        // Sending another heap's block may read the inbox, and blocks filed from it land in the bin
        // being shrunk. Setting the smaller capacity after that let pushes run past the bin.
        let test = TestRegion::new(256, 2);
        let region = test.region();
        let (heap, other) = (region.heap().unwrap(), region.heap().unwrap());
        let class = Class::for_size(ANYWHERE);
        assert!(class.reusable_anywhere(), "the bin keeps the other heap's blocks");
        let layout = layout(class.block_size(), 8);
        let max = class.bin_max();
        heap.data.set_capacity(class, max);
        heap.data.private.grown.set((max - class.bin_initial()) * class.block_size());
        // Blocks of this heap come back through its inbox once the other heap flushes them.
        let mine = alloc_many(&heap, layout, max);
        free_all(&other, layout, mine);
        other.flush();
        assert!(has_mail(&heap));
        // The bin fills with the other heap's blocks, which shrinking sends home.
        let theirs = alloc_many(&other, layout, max);
        free_all(&heap, layout, theirs);
        heap.data.bin(class).misses.set(0);
        heap.data.private.refills.set(MAINTENANCE_INTERVAL - 1);
        // SAFETY: the handle holds the claim.
        unsafe { heap.data.maintain(&region) };
        let capacity = usize::from(heap.data.bin(class).capacity.get());
        assert!(capacity < max, "the idle bin shrank");
        assert!(heap.data.bin_len(class) <= capacity);
        heap.validate();
        other.validate();
        heap.flush();
        other.flush();
        heap.flush();
        assert_eq!(used(&region), 0);
    }
    #[test]
    fn zeroed_requests_read_as_zeros_whether_memory_is_fresh_reused_or_cached() {
        let test = TestRegion::new(256, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let check = |block: NonNull<u8>, size: usize| {
            // SAFETY: the block holds `size` bytes.
            let bytes = unsafe { core::slice::from_raw_parts(block.as_ptr(), size) };
            assert!(bytes.iter().all(|&byte| byte == 0), "a zeroed block of {size} has data");
        };
        // Miri checks every byte it writes, so it takes the small sizes only.
        let sizes: &[usize] = if cfg!(miri) {
            &[24, 4096]
        } else {
            &[24, 2000, 4096, 20 << 10, 9 * CHUNK_SIZE + 5, 3 << 20]
        };
        for &size in sizes {
            let layout = layout(size, 8);
            for _ in 0..3 {
                let count = if size > CHUNK_SIZE { 3 } else { 40 };
                let blocks: Vec<_> =
                    (0..count).map(|_| heap.alloc_zeroed(layout).unwrap()).collect();
                for &block in &blocks {
                    check(block, size);
                    // SAFETY: the block holds `size` bytes.
                    unsafe { block.as_ptr().write_bytes(0xa5, size) };
                }
                free_all(&heap, layout, blocks);
                // The next round takes the same memory back, from the bin, the slab or the cache
                // of huge blocks, or after a flush from the chunk bitmap.
                if size > 4096 {
                    heap.flush();
                }
            }
        }
        heap.flush();
        heap.validate();
        assert_eq!(used(&region), 0);
    }
    #[test]
    fn blocks_are_as_aligned_as_their_layouts_ask() {
        let test = TestRegion::new(64, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        for size in [8, 24, 40, 56, 120, 136] {
            for align in [8, 16] {
                let blocks = alloc_many(&heap, layout(size, align), 50);
                assert!(blocks.iter().all(|block| block.as_ptr().addr() % align == 0));
                free_all(&heap, layout(size, align), blocks);
            }
        }
    }

    #[test]
    fn big_heaps_and_big_blocks_take_chunks_from_the_huge_page_zone() {
        // Four bitmap words: chunks 0 to 127 are the small zone and 128 up the huge one.
        let test = TestRegion::new(256, 2).with_huge_zone();
        let region = test.region();
        let (big, small) = (region.heap().unwrap(), region.heap().unwrap());
        let chunk = |block: NonNull<u8>| region.chunk_index(block.as_ptr());
        let blocks = layout(8192, 8);
        let first = alloc_many(&big, blocks, 8 * (HUGE_ZONE_CHUNKS - 4));
        assert!(first.iter().all(|&block| chunk(block) < 128), "a small heap uses small pages");
        let later = alloc_many(&big, blocks, 8 * 16);
        assert!(chunk(*later.last().unwrap()) >= 128, "a big heap moves to the huge page zone");
        let sparse = big.alloc(layout(20 << 10, 8)).unwrap();
        assert!(chunk(sparse) < 128, "a slab of several chunks of a big heap uses small pages");
        let run = big.alloc(layout(9 * CHUNK_SIZE, 8)).unwrap();
        assert!(chunk(run) >= 128, "a block of whole chunks of a big heap uses huge pages");
        let medium = small.alloc(layout(9 * CHUNK_SIZE, 8)).unwrap();
        assert!(chunk(medium) < 128, "and one of a small heap does not");
        let large = small.alloc(layout(HUGE_PAGE_CHUNKS * CHUNK_SIZE, 8)).unwrap();
        assert!(chunk(large) >= 128, "a block of a huge page or more uses huge pages");
        let next = small.alloc(layout(9 * CHUNK_SIZE, 8)).unwrap();
        assert!(chunk(next) >= 128, "a heap is big once it holds big blocks too");
        for block in first.into_iter().chain(later) {
            // SAFETY: allocated above with this layout.
            unsafe { big.dealloc(block, blocks) };
        }
        // SAFETY: as above.
        unsafe {
            big.dealloc(sparse, layout(20 << 10, 8));
            big.dealloc(run, layout(9 * CHUNK_SIZE, 8));
        }
        big.flush();
        let again = big.alloc(blocks).unwrap();
        assert!(chunk(again) < 128, "a heap that shrank to a quarter goes back to small pages");
        big.validate();
        small.validate();
    }

    #[test]
    fn a_dropped_heaps_slot_and_slabs_pass_to_the_next_claimant() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (first, second) = (region.heap().unwrap(), region.heap().unwrap());
        assert!(region.heap().is_none(), "both slots are taken");
        assert_eq!(region.stats().heaps_in_use, 2);

        let layout = layout(48, 8);
        let blocks = alloc_many(&first, layout, 100);
        drop(first);
        let successor = region.heap().unwrap();
        // The successor owns the departed heap's slabs, so these are local frees.
        free_all(&successor, layout, blocks);
        successor.validate();
        drop((second, successor));
        assert_eq!(region.stats().heaps_in_use, 0);
    }

    #[test]
    fn a_thread_without_a_heap_can_still_free() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let (small, huge) = (layout(300, 8), layout(2 << 20, 1));
        let blocks = alloc_many(&heap, small, 50);
        let big = heap.alloc(huge).unwrap();
        for block in blocks {
            // SAFETY: allocated with this layout and freed once.
            unsafe { region.dealloc(block, small) };
        }
        // SAFETY: as above.
        unsafe { region.dealloc(big, huge) };
        heap.validate();
        heap.flush();
        assert_eq!(used(&region), 0);
    }

    #[test]
    fn trimming_empties_idle_heaps() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(1000, 8);
        let blocks = alloc_many(&heap, layout, 200);
        free_all(&heap, layout, blocks);
        let held = used(&region);
        assert!(held > 0);

        // A heap that is in use is skipped, an idle one is flushed.
        region.trim();
        assert_eq!(used(&region), held);
        drop(heap);
        region.trim();
        assert_eq!(used(&region), 0);
    }

    #[test]
    fn threads_that_start_together_each_get_a_heap() {
        // Found by the benchmarks. Sixteen threads starting at once could make a claim fail
        // although there were free slots, because the search window lagged behind the claims.
        const THREADS: usize = 16;
        for _ in 0..iterations(300) {
            let test = TestRegion::new(1, THREADS);
            let region = test.region();
            let barrier = std::sync::Barrier::new(THREADS);
            let claimed: usize = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            let heap = region.heap();
                            // Every thread holds its claim until all of them have tried.
                            barrier.wait();
                            heap.is_some()
                        })
                    })
                    .collect();
                handles.into_iter().map(|handle| usize::from(handle.join().unwrap())).sum()
            });
            assert_eq!(claimed, THREADS, "a claim failed with free slots");
        }
    }

    #[test]
    fn threads_that_free_each_others_blocks_lose_nothing() {
        const THREADS: usize = 4;
        let test = TestRegion::new(256, THREADS);
        let region = test.region();
        let rounds = iterations(200);
        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..THREADS).map(|_| std::sync::mpsc::channel::<Vec<(usize, usize)>>()).unzip();
        std::thread::scope(|scope| {
            for (id, receiver) in receivers.into_iter().enumerate() {
                let next = senders[(id + 1) % THREADS].clone();
                let region = &region;
                scope.spawn(move || {
                    let heap = region.heap().unwrap();
                    let mut rng = Rng::new(id as u64);
                    for _ in 0..rounds {
                        let batch: Vec<_> = (0..64)
                            .map(|_| {
                                let size = rng.between(1, 2000);
                                let block = heap.alloc(layout(size, 8)).unwrap();
                                // SAFETY: the block holds `size` bytes and is ours.
                                unsafe { block.as_ptr().write_bytes(id as u8, size) };
                                (block.as_ptr().addr(), size)
                            })
                            .collect();
                        next.send(batch).unwrap();
                        let batch = receiver.recv().unwrap();
                        for (addr, size) in batch {
                            let block = region.chunk_ptr(0).with_addr(addr);
                            // SAFETY: the sender allocated the block with this size and gave it up.
                            unsafe { heap.dealloc(NonNull::new(block).unwrap(), layout(size, 8)) };
                        }
                    }
                    heap
                });
            }
            drop(senders);
        });
        region.trim();
        assert_eq!(used(&region), 0, "every block came home");
    }

    #[test]
    fn oversized_alignment_is_refused() {
        let test = TestRegion::new(16, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        assert!(heap.alloc(layout(8, 2 * CHUNK_SIZE)).is_none());
        assert!(heap.alloc(layout(8, CHUNK_SIZE)).is_some());
    }

    #[test]
    fn an_all_zero_slot_is_an_unclaimed_heap() {
        // SAFETY: the claim under test. Miri verifies that no field is read while invalid.
        let data: HeapData = unsafe { MaybeUninit::zeroed().assume_init() };
        let test = TestRegion::new(4, 1);
        let region = test.region();
        assert!(!data.is_claimed());
        assert!(data.claim(0, &region));
        assert!(!data.claim(0, &region), "a claimed slot cannot be claimed again");
        data.release_claim();
        assert!(data.claim(0, &region));
    }

    #[test]
    fn random_traffic_across_three_heaps_keeps_every_invariant() {
        for seed in 0..iterations(20) as u64 {
            let test = TestRegion::new(160, 3);
            random_traffic(&test.region(), seed, iterations(4_000));
        }
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use super::*;
    use crate::testing::TestRegion;
    use core::mem;
    use loom::thread;

    /// A pointer that may be moved to another thread.
    struct SendPtr(NonNull<u8>);
    // SAFETY: the pointers are only handed to allocator calls and compared, never dereferenced
    // concurrently.
    unsafe impl Send for SendPtr {}

    /// Lets a loom thread, which must be `'static`, borrow a region.
    ///
    /// # Safety
    ///
    /// Every thread that uses the result must be joined before the region is dropped.
    unsafe fn forever<'a>(region: &'a Region<'a>) -> &'static Region<'static> {
        // SAFETY: forwarded.
        unsafe { mem::transmute(region) }
    }

    /// 8 KiB blocks, which come eight to a slab.
    fn layout() -> Layout {
        Layout::from_size_align(8192, 8).unwrap()
    }

    #[test]
    fn two_threads_never_claim_the_same_heap() {
        loom::model(|| {
            let test = TestRegion::new(1, 1);
            let region = test.region();
            // SAFETY: both threads are joined below, before the region goes away.
            let shared = unsafe { forever(&region) };
            let other = thread::spawn(move || shared.heap());
            let mine = region.heap();
            let theirs = other.join().unwrap();
            // The winners hold their handles until here, so the single slot cannot be reused.
            assert!(mine.is_some() ^ theirs.is_some());
        });
    }

    #[test]
    fn concurrent_claims_each_find_a_free_slot() {
        loom::model(|| {
            let test = TestRegion::new(1, 2);
            let region = test.region();
            // SAFETY: both threads are joined below, before the region goes away.
            let shared = unsafe { forever(&region) };
            let other = thread::spawn(move || shared.heap());
            let mine = region.heap();
            let theirs = other.join().unwrap();
            assert!(mine.is_some() && theirs.is_some(), "a claim failed with a free slot");
        });
    }

    #[test]
    fn blocks_sent_while_the_owner_allocates_are_never_lost() {
        loom::model(|| {
            // Two chunks, so the second slab takes the last of them and the region is full.
            let test = TestRegion::new(2, 1);
            let region = test.region();
            let owner = region.heap().unwrap();
            let layout = layout();
            let blocks: std::vec::Vec<_> =
                (0..16).map(|_| owner.alloc(layout).expect("sixteen blocks fit")).collect();
            let (x, y) = (blocks[2], blocks[5]);
            let (x_send, y_send) = (SendPtr(x), SendPtr(y));

            // SAFETY: the freeing thread is joined below, before the region goes away.
            let shared = unsafe { forever(&region) };
            let freer = thread::spawn(move || {
                // SAFETY: both blocks are live, allocated with this layout, and freed once.
                unsafe {
                    shared.dealloc(x_send.0, layout);
                    shared.dealloc(y_send.0, layout);
                }
            });
            // This races with the frees. Whatever it finds must be one of the two freed blocks.
            let mut got = std::vec![owner.alloc(layout)];
            freer.join().unwrap();

            // Both frees have completed, so both blocks must be recoverable.
            while let Some(block) = owner.alloc(layout) {
                got.push(Some(block));
            }
            let mut found: std::vec::Vec<_> = got.into_iter().flatten().collect();
            found.sort();
            let mut expected = std::vec![x, y];
            expected.sort();
            assert_eq!(found, expected, "a freed block went missing or appeared twice");
        });
    }

    #[test]
    fn two_senders_and_the_owner_reading_its_inbox_lose_nothing() {
        loom::model(|| {
            let test = TestRegion::new(1, 1);
            let region = test.region();
            let owner = region.heap().unwrap();
            let layout = layout();
            let blocks: std::vec::Vec<_> =
                (0..8).map(|_| owner.alloc(layout).expect("eight blocks fit")).collect();
            let (x, y) = (SendPtr(blocks[0]), SendPtr(blocks[1]));
            // SAFETY: the threads are joined below, before the region goes away.
            let (one, two) = unsafe { (forever(&region), forever(&region)) };
            let first = thread::spawn(move || {
                let x = x;
                // SAFETY: the block is live, allocated with this layout, and freed once.
                unsafe { one.dealloc(x.0, layout) };
            });
            let second = thread::spawn(move || {
                let y = y;
                // SAFETY: as above.
                unsafe { two.dealloc(y.0, layout) };
            });
            // SAFETY: this thread holds the owner's claim.
            unsafe { owner.data.drain_inbox(&region) };
            first.join().unwrap();
            second.join().unwrap();
            // SAFETY: as above.
            unsafe { owner.data.drain_inbox(&region) };
            let mut seen = std::collections::BTreeSet::new();
            owner.validate_into(&mut seen);
            assert!(
                seen.contains(&blocks[0].as_ptr().addr()) &&
                    seen.contains(&blocks[1].as_ptr().addr())
            );
        });
    }
}
