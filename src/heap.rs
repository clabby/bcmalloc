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
//!                     +- own slab, or a class whose blocks share no cache line -> push on the bin
//!   free(ptr, layout) |
//!                     +- another heap's slab -> append to the outbox for that heap
//!                                                  |
//!                                                  +-> a full outbox becomes a message in the
//!                                                      owner's inbox, one compare-and-swap
//! ```
//!
//! A heap may keep a block of another heap's slab in its own bin and hand it out again, as
//! jemalloc and TCMalloc do, but only for classes whose blocks never share a cache line with other
//! blocks. Otherwise the reusing thread would write next to a block that the owner still uses,
//! which is the false sharing that Hoard's rule of returning blocks to their owner prevents. Blocks
//! of the other classes always go home.
//!
//! A bin that fills up gives its oldest half back: blocks of this heap's slabs go to the slab, and
//! the others go to their owners.
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

use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::Cell,
    fmt,
    marker::PhantomData,
    ptr,
    ptr::NonNull,
};

use crate::{
    allocator::{Backend, allocator},
    cache::CacheLine,
    chunks::CHUNK_SHIFT,
    region::{Entry, Region},
    size_class::{BIN_SLOTS, CLASSES, Class, Request, classify},
    slab::{Block, List, Owned, Slab},
    sync::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
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
const OUTBOXES: usize = 16;

/// An outbox is posted once it holds this many bytes of blocks, so that big blocks are not held
/// back for long. A single block this large is sent on its own at once.
const OUTBOX_BYTES: usize = 64 << 10;

/// How many refills pass between maintenance steps, which post the outboxes so that no block waits
/// in one for long.
const MAINTENANCE_INTERVAL: u32 = 64;

/// Blocks on their way to the heap that owns their slabs.
///
/// A message is an ordinary block of [`MESSAGE_CLASS`] from the sender's heap. Its first word is
/// the inbox link, as for a single block, so both kinds of node share the link's place.
#[repr(C)]
struct Message {
    next: *mut Block,
    count: usize,
    blocks: [*mut Block; MESSAGE_BLOCKS],
}

const _: () = assert!(size_of::<Message>() == MESSAGE_BYTES);
const _: () = assert!(MESSAGE_CLASS.block_size() == MESSAGE_BYTES);

/// A stack of free block pointers for one class. See the [module documentation](self).
///
/// The stack lives in the heap's slot array and grows down. It is empty when `top` is at the end of
/// its range and full when it is at the start. Both ends fit in 16 bits of address, because a bin
/// is at most 1 KiB long, so the checks compare the low bits of `top` only and the bin fits in one
/// load of 16 bytes.
struct Bin {
    top: Cell<*mut *mut Block>,
    empty: Cell<u16>,
    full: Cell<u16>,
    /// Whether this bin may hold blocks of other heaps' slabs.
    anywhere: Cell<bool>,
}

/// Blocks of other heaps' slabs that this heap has freed and not posted yet, all for one heap.
struct Outbox {
    dest: Cell<u32>,
    /// The message being filled, or null. It stays after a post that sent its one block alone.
    message: Cell<*mut Message>,
    bytes: Cell<u32>,
}

/// The slab lists of a size class. The active list is tracked from both ends.
struct Lists {
    active: Cell<*const Slab>,
    active_last: Cell<*const Slab>,
    full: Cell<*const Slab>,
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
    /// The start of the managed memory, the slab descriptors and the chunk map of the region this
    /// slot was claimed from, so that the fast paths reach them without loading the region. They
    /// are set at every claim and valid while the claim lasts.
    base: Cell<*mut u8>,
    map: Cell<*const AtomicU64>,
    slabs: Cell<*const Slab>,
    region: Cell<*const Region<'static>>,
    cursor: Cell<usize>,
    bins: [Bin; CLASSES],
    slots: [Cell<*mut Block>; BIN_SLOTS],
    outboxes: [Outbox; OUTBOXES],
    classes: [Lists; CLASSES],
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
                base: Cell::new(ptr::null_mut()),
                map: Cell::new(ptr::null()),
                slabs: Cell::new(ptr::null()),
                region: Cell::new(ptr::null()),
                cursor: Cell::new(0),
                bins: [const {
                    Bin {
                        top: Cell::new(ptr::null_mut()),
                        empty: Cell::new(0),
                        full: Cell::new(0),
                        anywhere: Cell::new(false),
                    }
                }; CLASSES],
                slots: [const { Cell::new(ptr::null_mut()) }; BIN_SLOTS],
                outboxes: [const {
                    Outbox {
                        dest: Cell::new(0),
                        message: Cell::new(ptr::null_mut()),
                        bytes: Cell::new(0),
                    }
                }; OUTBOXES],
                classes: [const {
                    Lists {
                        active: Cell::new(ptr::null()),
                        active_last: Cell::new(ptr::null()),
                        full: Cell::new(ptr::null()),
                    }
                }; CLASSES],
            },
        }
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
            private.slabs.set(region.slabs_ptr());
            private.map.set(region.map_ptr());
            self.init_bins();
        }
        won
    }

    /// Points every bin at its range of the slot array. A slot that was claimed before has empty
    /// bins at the same place, so this only matters the first time.
    fn init_bins(&self) {
        let slots = self.slots();
        for (index, bin) in self.private.bins.iter().enumerate() {
            let class = Class::from_index(index);
            let start = slots.wrapping_add(class.bin_start());
            let end = start.wrapping_add(class.bin_capacity());
            if bin.top.get().is_null() {
                bin.top.set(end);
            }
            debug_assert_eq!(bin.top.get(), end, "a released heap left blocks in a bin");
            bin.empty.set(end.addr() as u16);
            bin.full.set(start.addr() as u16);
            bin.anywhere.set(class.reusable_anywhere());
        }
    }

    /// The first slot of the slot array, with provenance over all of it. The slots are cells, so
    /// writing through the pointer is allowed.
    const fn slots(&self) -> *mut *mut Block {
        self.private.slots.as_ptr().cast::<*mut Block>().cast_mut()
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

    /// Pops a block off the bin of `class`.
    #[inline(always)]
    fn pop(&self, class: Class) -> Option<*mut Block> {
        let bin = &self.private.bins[class.index()];
        let top = bin.top.get();
        if top.addr() as u16 == bin.empty.get() {
            return None;
        }
        bin.top.set(top.wrapping_add(1));
        // SAFETY: slots between the top and the end of a bin hold blocks.
        Some(unsafe { *top })
    }

    /// Pushes a block onto the bin of `class`, or returns false if the bin is full.
    #[inline(always)]
    fn push(&self, class: Class, block: *mut Block) -> bool {
        let bin = &self.private.bins[class.index()];
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
        let end = self.slots().wrapping_add(class.bin_start() + class.bin_capacity());
        (end.addr() - self.private.bins[class.index()].top.get().addr()) / size_of::<usize>()
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
            let class = Class::from_table(layout.size());
            if let Some(block) = self.pop(class) {
                return block.cast();
            }
            // SAFETY: forwarded.
            return unsafe { self.refill(class) }.cast();
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_general(layout) }
    }

    /// Allocates anything that the fast path in [`HeapData::alloc`] does not take.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(never)]
    unsafe fn alloc_general(&self, layout: Layout) -> *mut u8 {
        match classify(layout) {
            Some(Request::Small(class)) => self
                .pop(class)
                // SAFETY: forwarded.
                .unwrap_or_else(|| unsafe { self.refill(class) })
                .cast(),
            Some(Request::Huge { chunks }) => {
                // SAFETY: the claim keeps the region alive.
                let region = unsafe { self.region() };
                let block = self.alloc_huge(region, chunks);
                if !block.is_null() {
                    return block;
                }
                // The region looks full, but this heap may be sitting on empty slabs. Give them
                // back and retry.
                // SAFETY: the caller holds the claim.
                unsafe { self.flush(region) };
                self.alloc_huge(region, chunks)
            }
            None => ptr::null_mut(),
        }
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
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        if !self.shared.inbox.load(Ordering::Relaxed).is_null() {
            // SAFETY: the caller holds the claim.
            unsafe { self.drain_inbox(region) };
            if let Some(block) = self.pop(class) {
                return block;
            }
        }
        // SAFETY: as above.
        unsafe { self.maintain(region) };
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
        }
    }

    /// Moves blocks from the active slabs of `class` into its bin, which must be empty, until the
    /// bin is half full. Returns how many it moved, which is zero only if the region is out of
    /// room.
    fn fill(&self, region: &Region<'_>, class: Class) -> usize {
        let bin = &self.private.bins[class.index()];
        let want = (class.bin_capacity() / 2).max(1);
        let mut top = bin.top.get();
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
            got += slab.take(want - got, |block| {
                top = top.wrapping_sub(1);
                // SAFETY: the bin was empty and takes at most half its capacity.
                unsafe { *top = block };
            });
            if slab.is_exhausted() {
                self.move_to(slab, List::Full);
            }
        }
        bin.top.set(top);
        got
    }

    /// Takes a fresh slab of `class` from the chunk allocator and puts it at the front of the
    /// active list. Returns false if the region has no room.
    unsafe fn add_slab(&self, region: &Region<'_>, class: Class) -> bool {
        let chunks = class.slab_chunks();
        let Some(first) = region.alloc_chunks(chunks, chunks, self.private.cursor.get()) else {
            return false;
        };
        region.set_owner(first, chunks, self.private.id.get(), class);
        let slab = region.slab(first);
        // SAFETY: the chunks were just allocated to us, so nobody else uses this descriptor.
        let owned = unsafe { slab.owned() };
        // SAFETY: the chunks are a slab-sized, slab-aligned run that is ours alone.
        unsafe { owned.init(class, region.chunk_ptr(first)) };
        self.push_front(owned, List::Active);
        if chunks == 1 {
            self.private.cursor.set(first / 64);
        }
        true
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
        if classify(old) == classify(new) {
            return ptr.as_ptr();
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
        if Class::is_table_request(layout) {
            let class = Class::from_table(layout.size());
            // SAFETY: forwarded.
            unsafe { self.free_small(ptr.as_ptr().cast(), class) };
        } else {
            // SAFETY: forwarded.
            unsafe { self.dealloc_general(ptr, layout) };
        }
    }

    /// Frees a block of a slab of `class`.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc).
    #[inline(always)]
    unsafe fn free_small(&self, block: *mut Block, class: Class) {
        // Testing the owner first keeps the common case, a block of this heap, on one branch that
        // predicts well. Whether the class allows reuse depends on the size, which is as random as
        // the program's requests.
        let owner = self.entry_of(block.cast()).owner();
        if owner != self.private.id.get() && !self.private.bins[class.index()].anywhere.get() {
            // SAFETY: forwarded.
            return unsafe { self.send(block, class, owner) };
        }
        if !self.push(class, block) {
            // SAFETY: forwarded.
            unsafe { self.free_full(block, class) };
        }
    }

    #[inline(never)]
    unsafe fn dealloc_general(&self, ptr: NonNull<u8>, layout: Layout) {
        match classify(layout) {
            // SAFETY: forwarded.
            Some(Request::Small(class)) => unsafe { self.free_small(ptr.as_ptr().cast(), class) },
            // SAFETY: the claim keeps the region alive, and the caller vouches for the block.
            _ => unsafe { self.region().dealloc(ptr, layout) },
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
        loop {
            // SAFETY: forwarded.
            unsafe { self.flush_bin(class, class.bin_capacity() / 2) };
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
        let bin = &self.private.bins[class.index()];
        let len = self.bin_len(class);
        if len <= keep {
            return;
        }
        let gone = len - keep;
        // The oldest blocks are at the end of the bin. Move them out first and the rest down, so
        // that the bin is consistent if giving the blocks back allocates or frees again.
        let mut out = [ptr::null_mut::<Block>(); 128];
        let top = bin.top.get();
        // SAFETY: the bin holds `len` blocks from `top`, and `out` holds a bin's capacity.
        unsafe {
            ptr::copy_nonoverlapping(top.add(keep), out.as_mut_ptr(), gone);
            ptr::copy(top, top.add(gone), keep);
        }
        bin.top.set(top.wrapping_add(gone));
        // SAFETY: the blocks were in the bin, so they are in use and ours to give back.
        unsafe { self.give_back(region, class, &out[..gone], false) };
    }

    /// Gives blocks of `class` that this heap holds back: blocks of its own slabs to the slab, and
    /// the others to their owners, through the outboxes or, if `directly`, on their own. Blocks
    /// that follow each other in one slab go back as one chain, which looks the slab up once.
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
        let mut run: Option<Owned<'_>> = None;
        let (mut head, mut tail, mut count) = (ptr::null_mut::<Block>(), ptr::null_mut(), 0);
        for &block in blocks {
            if let Some(slab) = run {
                if block.addr().wrapping_sub(slab.start().addr()) < span {
                    // SAFETY: the block is ours to give back, and the chain is ours to build.
                    unsafe { (*block).next = head };
                    head = block;
                    count += 1;
                    continue;
                }
                // SAFETY: the chain holds `count` blocks of the slab from `head` to `tail`.
                unsafe { self.return_chain(region, slab, head, tail, count) };
                run = None;
            }
            let owner = self.entry_of(block.cast()).owner();
            if owner != id {
                if directly {
                    // SAFETY: the block belongs to `owner` and is given up.
                    unsafe { region.heap_slot(owner).post_block(block) };
                } else {
                    // SAFETY: as above.
                    unsafe { self.send(block, class, owner) };
                }
                continue;
            }
            // SAFETY: the chunk map says the slab is ours.
            run = Some(unsafe {
                region.slab_of(NonNull::new_unchecked(block.cast()), class).owned()
            });
            (head, tail, count) = (block, block, 1);
        }
        if let Some(slab) = run {
            // SAFETY: as above.
            unsafe { self.return_chain(region, slab, head, tail, count) };
        }
    }

    /// Returns a chain of blocks to one of this heap's slabs, and moves or releases the slab as
    /// its occupancy says.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the chain must hold `count` in-use blocks of the slab,
    /// linked from `head` to `tail`, which are given up.
    unsafe fn return_chain(
        &self,
        region: &Region<'_>,
        slab: Owned<'_>,
        head: *mut Block,
        tail: *mut Block,
        count: u32,
    ) {
        // SAFETY: forwarded.
        unsafe { slab.push_chain(head, tail, count) };
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
        // SAFETY: the chunk map says the slab is ours.
        let slab = unsafe { region.slab_of(NonNull::new_unchecked(block.cast()), class).owned() };
        // SAFETY: a single block is a chain whose head and tail are the same.
        unsafe { self.return_chain(region, slab, block, block, 1) };
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
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let size = class.block_size();
        let outbox = &self.private.outboxes[owner as usize % OUTBOXES];
        let mut message = outbox.message.get();
        if !message.is_null() && outbox.dest.get() != owner {
            // SAFETY: the caller holds the claim.
            unsafe { self.post_outbox(region, outbox) };
            message = outbox.message.get();
        }
        if message.is_null() {
            if size >= OUTBOX_BYTES {
                // SAFETY: the caller gives the block up, and it belongs to `owner`.
                return unsafe { region.heap_slot(owner).post_block(block) };
            }
            message = self.alloc_message(region);
            if message.is_null() {
                // SAFETY: as above.
                return unsafe { region.heap_slot(owner).post_block(block) };
            }
            // SAFETY: the message is a fresh block of ours.
            unsafe { (*message).count = 0 };
            outbox.message.set(message);
            outbox.bytes.set(0);
        }
        outbox.dest.set(owner);
        // SAFETY: the message is ours until it is posted, and it is never left full.
        let count = unsafe {
            let count = (*message).count;
            (*message).blocks[count] = block;
            (*message).count = count + 1;
            count + 1
        };
        let bytes = outbox.bytes.get() as usize + size;
        outbox.bytes.set(bytes as u32);
        if count == MESSAGE_BLOCKS || bytes >= OUTBOX_BYTES {
            // SAFETY: the caller holds the claim.
            unsafe { self.post_outbox(region, outbox) };
        }
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

    /// Takes everything in the inbox and files each block in its bin, or in its slab if the bin is
    /// full. Messages are then freed like any other block.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn drain_inbox(&self, region: &Region<'_>) {
        let mut node = self.shared.inbox.swap(ptr::null_mut(), Ordering::Acquire);
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
                // SAFETY: as above, a message holds `count` blocks of our slabs.
                unsafe {
                    for index in 0..(*message).count {
                        self.file(region, (*message).blocks[index]);
                    }
                    self.free_small(message.cast(), MESSAGE_CLASS);
                }
            }
            node = next;
        }
    }

    /// Takes in a block of one of this heap's slabs that another thread sent back.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be an in-use block of one of this heap's
    /// slabs, which is given up.
    unsafe fn file(&self, region: &Region<'_>, block: *mut Block) {
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
        let first = region.chunk_index(slab.start());
        region.free_chunks(first, slab.class().slab_chunks());
        region.purge_if_due();
        // Prefer the lowest free word next time, so slabs stay packed towards the bottom.
        self.private.cursor.set(self.private.cursor.get().min(first / 64));
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
        region.alloc_chunks(chunks, 1, 0).map_or(ptr::null_mut(), |first| region.chunk_ptr(first))
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
            entry
        };
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            let size = class.block_size();
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
                    let state = owned.inspect();
                    assert!(state.bumped <= state.capacity);
                    assert!(state.free_len <= state.bumped);
                    let mut block = state.free;
                    let mut count = 0;
                    while !block.is_null() {
                        let offset = block.addr().wrapping_sub(state.start.addr());
                        assert!(
                            offset.is_multiple_of(size) && offset / size < state.bumped as usize,
                            "the free list holds a block outside its slab"
                        );
                        claim(block, "a free list");
                        count += 1;
                        // SAFETY: the block is unused slab memory whose first word is a link.
                        block = unsafe { (*block).next };
                    }
                    assert_eq!(
                        count, state.free_len,
                        "free list length disagrees with its counter"
                    );
                    prev = current;
                    current = owned.next();
                }
                if list == List::Active {
                    assert!(ptr::eq(lists.active_last.get(), prev), "wrong last active slab");
                }
            }
            let bin = &self.private.bins[index];
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
        }
        for outbox in &self.private.outboxes {
            let message = outbox.message.get();
            if message.is_null() {
                continue;
            }
            assert_eq!(check(message.cast(), "an outbox").class(), MESSAGE_CLASS);
            claim(message.cast(), "an outbox");
            // SAFETY: the message is ours until it is posted.
            let blocks = unsafe { &(&(*message).blocks)[..(*message).count] };
            assert!(blocks.len() < MESSAGE_BLOCKS, "a full message was not posted");
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
                for &block in unsafe { &(&(*message).blocks)[..(*message).count] } {
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
        let block = self.alloc(layout)?;
        // SAFETY: the block holds at least `layout.size()` bytes and is ours.
        unsafe { block.as_ptr().write_bytes(0, layout.size()) };
        Some(block)
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

    /// Resizes a block, returning `None` (and leaving the block untouched) if there is no room.
    ///
    /// A block whose new size falls in the same size class, or the same number of chunks for huge
    /// blocks, is returned as is. Otherwise the contents are copied to a new block.
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
    use core::{
        alloc::Layout,
        mem::{MaybeUninit, align_of, offset_of, size_of},
        ptr::NonNull,
    };
    use std::vec::Vec;

    use super::*;
    use crate::{
        chunks::CHUNK_SIZE,
        testing::{Harness, Rng, TestRegion, iterations, random_traffic},
    };

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
        assert!(MESSAGE_CLASS.reusable_anywhere(), "messages are recycled by whoever reads them");
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
    fn a_full_bin_gives_back_its_older_half() {
        let test = TestRegion::new(16, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let class = Class::for_size(ANYWHERE);
        let capacity = class.bin_capacity();
        let layout = layout(ANYWHERE, 8);
        let blocks = alloc_many(&heap, layout, 2 * capacity);
        let newest = *blocks.last().unwrap();
        free_all(&heap, layout, blocks);
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
        let same = unsafe { heap.realloc(block, old, 110) }.unwrap();
        assert_eq!(same, block, "100 and 110 bytes share a size class");

        // SAFETY: the block is still live, now with the 110-byte layout.
        let grown = unsafe { heap.realloc(same, layout(110, 8), 5000) }.unwrap();
        assert_ne!(grown, block);
        // SAFETY: the grown block holds at least 100 initialised bytes.
        let copied = unsafe { core::slice::from_raw_parts(grown.as_ptr(), 100) };
        assert!(copied.iter().all(|&byte| byte == 0xAB));
        // SAFETY: allocated by the call above.
        unsafe { heap.dealloc(grown, layout(5000, 8)) };
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
        assert!(data.claim(3, &region));
        assert!(!data.claim(4, &region), "a claimed slot cannot be claimed again");
        data.release_claim();
        assert!(data.claim(5, &region));
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
    use core::mem;

    use loom::thread;

    use super::*;
    use crate::testing::TestRegion;

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
