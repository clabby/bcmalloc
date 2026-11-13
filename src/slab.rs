//! A slab, which is a run of chunks carved into equal blocks of one size class.
//!
//! Every slab has a descriptor, kept outside the slab so that user memory is never metadata. The
//! descriptor has two halves on separate cache lines. One half is state that only the owning heap
//! touches. The other is state that any thread may touch to free a block.
//!
//! ```text
//!   slab memory, one size class (64-byte blocks here)
//!   +-------+-------+-------+-------+-------+-------+-------+-------+--------------+
//!   | block | block | block | block | block | block | block | block |  never used  |
//!   +-------+-------+-------+-------+-------+-------+-------+-------+--------------+
//!   0                                             bumped --^                capacity
//!
//!   descriptor, cache line 1 (owner only)      descriptor, cache line 2 (shared)
//!   +-------------------------------------+    +---------------------------------+
//!   | free       -> block -> block -> nil |    | remote -> block -> block -> nil |
//!   | local_free -> block -> nil          |    |           + FULL and QUEUED     |
//!   | used, bumped, capacity, class       |    | owner, notified                 |
//!   | next, prev, list                    |    +---------------------------------+
//!   +-------------------------------------+
//! ```
//!
//! # Free lists
//!
//! `free` is the list the allocator pops from, and popping it takes no atomics. `local_free`
//! collects blocks freed by the owning thread. It is spliced into `free` when `free` runs dry, so
//! the fast path never has to look at it. `remote` collects blocks freed by other threads. It is a
//! push-only atomic stack that the owner empties in one swap, so it cannot suffer the ABA problem
//! that a stack with pop-by-anyone would.
//!
//! Blocks that have never been handed out are on no list. The `bumped` frontier marks how many
//! blocks have been threaded onto `free` so far, so the slab touches fresh memory a few blocks at a
//! time instead of all at once.
//!
//! # The remote word
//!
//! A full slab leaves the owner's allocation path, so the owner has no reason to look at it again.
//! If another thread frees a block into it, the owner needs to hear about that exactly once. The
//! `remote` word carries the whole protocol. The list head sits in the high bits and two flags sit
//! in the low bits, which are free because blocks are 16-byte aligned.
//!
//! ```text
//!         owner calls try_mark_full         any thread calls push_remote
//!         (only works on an empty word)     (returns true when it turns FULL into QUEUED)
//!   +--------+ -----------------------> +------+ ---------------------------> +--------+
//!   | NORMAL |                          | FULL |                              | QUEUED |
//!   +--------+ <----------------------- +------+                              +--------+
//!       ^        owner calls clear_full                                           |
//!       |                                                                         |
//!       +-------------------- owner calls collect with clear ---------------------+
//! ```
//!
//! The list and the flags change in the same compare-and-swap. A pusher therefore learns atomically
//! whether it must notify the owner, and the owner can only mark the slab full when no block is
//! pending. No fences and no second look at the descriptor are needed. That matters because the
//! last block of a slab may be freed remotely, after which the owner is free to recycle the slab.
//!
//! A queued slab cannot be recycled until the owner has processed the notification, because the
//! notification still points at it. The sticky `QUEUED` flag records that.

use core::{cell::Cell, fmt, ptr};

use crate::{
    size_class::Class,
    sync::{AtomicPtr, AtomicU32, Ordering},
};

/// The slab is on its owner's full list, and the next remote free must notify the owner.
const FULL: usize = 0b01;

/// A remote free has notified the owner, who has not processed the notification yet.
const QUEUED: usize = 0b10;

/// The bits of the remote word that are flags rather than list head.
const FLAGS: usize = 0b11;

/// The flag bits of a remote word.
fn flags(word: *mut Block) -> usize {
    word.addr() & FLAGS
}

/// The list head of a remote word, with the flag bits cleared.
fn list(word: *mut Block) -> *mut Block {
    word.map_addr(|addr| addr & !FLAGS)
}

/// A remote word made of a list head and flag bits. Tagging the pointer keeps its provenance, which
/// storing it in an integer would lose.
fn tagged(list: *mut Block, flags: usize) -> *mut Block {
    list.map_addr(|addr| addr | flags)
}

/// At most this many blocks are threaded onto the free list at a time.
const EXTEND_BLOCKS: usize = 64;

/// A target for how much fresh memory one extension touches.
const EXTEND_BYTES: usize = 4096;

/// A free block, seen as a link in an intrusive list. Blocks are at least 16 bytes.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct Block {
    next: *mut Self,
}

/// Which of the owner's slab lists a slab is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum List {
    /// On neither list. This is an unused descriptor or a slab in the middle of a move.
    None = 0,
    /// Has free blocks or may have, and allocation draws from here.
    Active = 1,
    /// Every block is allocated, or a notification is pending.
    Full = 2,
}

/// Descriptor state that only the owning heap reads or writes.
#[repr(align(64))]
struct Local {
    free: Cell<*mut Block>,
    local_free: Cell<*mut Block>,
    next: Cell<*const Slab>,
    prev: Cell<*const Slab>,
    start: Cell<*mut u8>,
    used: Cell<u32>,
    bumped: Cell<u32>,
    capacity: Cell<u32>,
    class: Cell<u8>,
    list: Cell<List>,
}

/// Descriptor state any thread may touch.
#[derive(Debug)]
#[repr(align(64))]
struct Shared {
    remote: AtomicPtr<Block>,
    owner: AtomicU32,
    notified: AtomicPtr<Slab>,
}

/// The descriptor of one slab. See the [module documentation](self).
///
/// An all-zero descriptor is a valid, unused one, which is what lets the region hand out
/// descriptors straight from zeroed memory.
pub(crate) struct Slab {
    local: Local,
    shared: Shared,
}

impl fmt::Debug for Slab {
    /// Formats only the shared half. The owner-only half may be changing on another thread.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slab").field("shared", &self.shared).finish_non_exhaustive()
    }
}

// SAFETY: the owner-only half is reachable only through [`Owned`], whose constructor is unsafe and
// requires exclusive ownership. The other half is atomic.
unsafe impl Sync for Slab {}

impl Slab {
    /// An unused descriptor.
    #[cfg(any(test, feature = "testing"))]
    #[allow(clippy::missing_const_for_fn, reason = "loom atomics have no const constructors")]
    pub(crate) fn new() -> Self {
        Self {
            local: Local {
                free: Cell::new(ptr::null_mut()),
                local_free: Cell::new(ptr::null_mut()),
                next: Cell::new(ptr::null()),
                prev: Cell::new(ptr::null()),
                start: Cell::new(ptr::null_mut()),
                used: Cell::new(0),
                bumped: Cell::new(0),
                capacity: Cell::new(0),
                class: Cell::new(0),
                list: Cell::new(List::None),
            },
            shared: Shared {
                remote: AtomicPtr::new(ptr::null_mut()),
                owner: AtomicU32::new(0),
                notified: AtomicPtr::new(ptr::null_mut()),
            },
        }
    }

    /// Gives access to the owner-only half.
    ///
    /// # Safety
    ///
    /// The caller must be the only thread using the owner-only half. That means it holds the heap
    /// that owns this slab, or it is initialising an unowned descriptor.
    pub(crate) const unsafe fn owned(&self) -> Owned<'_> {
        Owned(self)
    }

    /// The id of the heap that owns this slab.
    pub(crate) fn owner(&self) -> u32 {
        self.shared.owner.load(Ordering::Relaxed)
    }

    /// Frees `block` onto the remote list from any thread.
    ///
    /// Returns whether the caller must now call [`enqueue`](Self::enqueue). That is true exactly
    /// when this push found the slab marked full, which makes it the push that turns the mark into
    /// a notification.
    ///
    /// # Safety
    ///
    /// `block` must be an allocated block of this slab that the caller has just stopped using.
    pub(crate) unsafe fn push_remote(&self, block: *mut Block) -> bool {
        let mut seen = self.shared.remote.load(Ordering::Relaxed);
        loop {
            // SAFETY: the caller owns the block until the compare-and-swap below publishes it.
            unsafe { (*block).next = list(seen) };
            let notify = flags(seen) & FULL != 0;
            let kept = if notify { QUEUED } else { flags(seen) & QUEUED };
            match self.shared.remote.compare_exchange_weak(
                seen,
                tagged(block, kept),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return notify,
                Err(actual) => seen = actual,
            }
        }
    }

    /// Pushes this slab onto its owner's stack of slabs to revisit.
    ///
    /// Call this once after [`push_remote`](Self::push_remote) returned true. The slab cannot be
    /// recycled before the owner pops it, so the pointer stays valid.
    pub(crate) fn enqueue(&self, stack: &AtomicPtr<Self>) {
        let this = ptr::from_ref(self).cast_mut();
        let mut head = stack.load(Ordering::Relaxed);
        loop {
            self.shared.notified.store(head, Ordering::Relaxed);
            match stack.compare_exchange_weak(head, this, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    /// Takes every slab pushed by [`enqueue`](Self::enqueue). Slabs come out most recent first.
    pub(crate) fn take_enqueued(stack: &AtomicPtr<Self>) -> *const Self {
        stack.swap(ptr::null_mut(), Ordering::Acquire)
    }

    /// The slab after this one in a list taken by [`take_enqueued`](Self::take_enqueued).
    pub(crate) fn next_enqueued(&self) -> *const Self {
        self.shared.notified.load(Ordering::Relaxed)
    }

    /// Atomically empties the remote list and returns its head, keeping the flags unless `clear`.
    fn take_remote(&self, clear: bool) -> *mut Block {
        let mut seen = self.shared.remote.load(Ordering::Relaxed);
        loop {
            if list(seen).is_null() && !(clear && !seen.is_null()) {
                return ptr::null_mut();
            }
            let kept = if clear { ptr::null_mut() } else { tagged(ptr::null_mut(), flags(seen)) };
            match self.shared.remote.compare_exchange_weak(
                seen,
                kept,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return list(seen),
                Err(actual) => seen = actual,
            }
        }
    }
}

/// The owner-only half of a [`Slab`], obtained from [`Slab::owned`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owned<'a>(&'a Slab);

impl<'a> Owned<'a> {
    const fn local(self) -> &'a Local {
        &self.0.local
    }

    /// The descriptor itself.
    pub(crate) const fn slab(self) -> &'a Slab {
        self.0
    }

    /// Prepares an unused descriptor for a fresh slab of `class` whose first block is at `start`.
    ///
    /// # Safety
    ///
    /// `start` must point to a slab-sized, suitably aligned region that nothing else uses.
    pub(crate) unsafe fn init(self, owner: u32, class: Class, start: *mut u8) {
        let local = self.local();
        local.free.set(ptr::null_mut());
        local.local_free.set(ptr::null_mut());
        local.next.set(ptr::null());
        local.prev.set(ptr::null());
        local.start.set(start);
        local.used.set(0);
        local.bumped.set(0);
        local.capacity.set(class.blocks_per_slab() as u32);
        local.class.set(class.index() as u8);
        local.list.set(List::None);
        self.0.shared.remote.store(ptr::null_mut(), Ordering::Relaxed);
        self.0.shared.owner.store(owner, Ordering::Relaxed);
    }

    /// The size class of this slab's blocks.
    pub(crate) fn class(self) -> Class {
        Class::from_index(usize::from(self.local().class.get()))
    }

    /// The first block of the slab, which is also the slab's start address.
    pub(crate) fn start(self) -> *mut u8 {
        self.local().start.get()
    }

    /// How many blocks are handed out or waiting on the remote list.
    pub(crate) fn used(self) -> u32 {
        self.local().used.get()
    }

    /// The list this slab is on.
    pub(crate) fn list(self) -> List {
        self.local().list.get()
    }

    /// Records which list this slab is on.
    pub(crate) fn set_list(self, list: List) {
        self.local().list.set(list);
    }

    /// The next slab on this slab's list.
    pub(crate) fn next(self) -> *const Slab {
        self.local().next.get()
    }

    /// Sets the next slab on this slab's list.
    pub(crate) fn set_next(self, next: *const Slab) {
        self.local().next.set(next);
    }

    /// The previous slab on this slab's list.
    pub(crate) fn prev(self) -> *const Slab {
        self.local().prev.get()
    }

    /// Sets the previous slab on this slab's list.
    pub(crate) fn set_prev(self, prev: *const Slab) {
        self.local().prev.set(prev);
    }

    /// Pops a block off the free list, or returns null if it is empty.
    pub(crate) fn pop(self) -> *mut Block {
        let local = self.local();
        let block = local.free.get();
        if !block.is_null() {
            // SAFETY: every block on the free list is unused slab memory whose first word is a
            // link.
            local.free.set(unsafe { (*block).next });
            local.used.set(local.used.get() + 1);
        }
        block
    }

    /// Frees a block from the owning thread.
    ///
    /// # Safety
    ///
    /// `block` must be an allocated block of this slab that the caller has stopped using.
    pub(crate) unsafe fn push_local(self, block: *mut Block) {
        let local = self.local();
        // SAFETY: the caller has given us the block.
        unsafe { (*block).next = local.local_free.get() };
        local.local_free.set(block);
        local.used.set(local.used.get() - 1);
    }

    /// Folds the local and remote free lists into the free list, and returns whether it has
    /// blocks. With `clear`, also resets the remote flags, acknowledging a notification.
    pub(crate) fn collect(self, clear: bool) -> bool {
        let local = self.local();
        if local.free.get().is_null() {
            local.free.set(local.local_free.replace(ptr::null_mut()));
        }
        let head = self.0.take_remote(clear);
        if !head.is_null() {
            let (mut tail, mut count) = (head, 1);
            // SAFETY: the remote list holds blocks of this slab, linked by their freeing threads
            // before the release that made them visible to our acquire.
            unsafe {
                while !(*tail).next.is_null() {
                    tail = (*tail).next;
                    count += 1;
                }
                (*tail).next = local.free.get();
            }
            local.free.set(head);
            local.used.set(local.used.get() - count);
        }
        !local.free.get().is_null()
    }

    /// Threads the next few never-used blocks onto the empty free list. Returns false when every
    /// block has been used at least once.
    pub(crate) fn extend(self) -> bool {
        let local = self.local();
        let (bumped, capacity) = (local.bumped.get() as usize, local.capacity.get() as usize);
        if bumped == capacity {
            return false;
        }
        let size = self.class().block_size();
        let count = (EXTEND_BYTES / size).clamp(1, EXTEND_BLOCKS).min(capacity - bumped);
        let mut head = local.free.get();
        // Link from the last block backwards so that allocation walks addresses upwards.
        for index in (bumped..bumped + count).rev() {
            // SAFETY: the block lies within the slab, because `capacity` blocks of `size` bytes
            // fit.
            let block = unsafe { local.start.get().add(index * size) }.cast::<Block>();
            // SAFETY: it is unused memory that we own.
            unsafe { (*block).next = head };
            head = block;
        }
        local.free.set(head);
        local.bumped.set((bumped + count) as u32);
        true
    }

    /// Marks the slab full so that the next remote free notifies the owner. Fails if a block is
    /// pending or a notification has not been processed, in which case the owner should collect.
    pub(crate) fn try_mark_full(self) -> bool {
        let full = tagged(ptr::null_mut(), FULL);
        self.0
            .shared
            .remote
            .compare_exchange(ptr::null_mut(), full, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Withdraws the full mark because the slab has free blocks again. A notification that has
    /// already been sent stays pending.
    pub(crate) fn clear_full(self) {
        let mut seen = self.0.shared.remote.load(Ordering::Relaxed);
        while flags(seen) & FULL != 0 {
            let cleared = seen.map_addr(|word| word & !FULL);
            match self.0.shared.remote.compare_exchange_weak(
                seen,
                cleared,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => seen = actual,
            }
        }
    }

    /// Whether a notification about this slab is waiting to be processed.
    pub(crate) fn is_queued(self) -> bool {
        flags(self.0.shared.remote.load(Ordering::Acquire)) & QUEUED != 0
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use std::vec::Vec;

    use super::*;
    use crate::testing::Buffer;

    /// A slab of `class` over its own memory, with the descriptor initialised.
    struct Fixture {
        slab: Slab,
        _memory: Buffer,
    }

    impl Fixture {
        fn new(block_size: usize) -> Self {
            let class = Class::for_size(block_size);
            let memory = Buffer::new(class.slab_chunks() << crate::chunks::CHUNK_SHIFT);
            let slab = Slab::new();
            // SAFETY: the buffer is slab sized and aligned, and we are the only user of the slab.
            unsafe { slab.owned().init(7, class, memory.as_ptr()) };
            Self { slab, _memory: memory }
        }

        const fn owned(&self) -> Owned<'_> {
            // SAFETY: tests drive each fixture from one thread.
            unsafe { self.slab.owned() }
        }

        /// Pops blocks until the slab is exhausted, extending as needed.
        fn exhaust(&self) -> Vec<*mut Block> {
            let owned = self.owned();
            let mut blocks = Vec::new();
            loop {
                let block = owned.pop();
                if !block.is_null() {
                    blocks.push(block);
                } else if !owned.extend() {
                    return blocks;
                }
            }
        }
    }

    #[test]
    fn a_fresh_slab_yields_every_block_once_in_address_order() {
        let fixture = Fixture::new(128);
        let blocks = fixture.exhaust();
        let size = fixture.owned().class().block_size();
        assert_eq!(blocks.len(), fixture.owned().class().blocks_per_slab());
        let start = fixture.owned().start();
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(*block, start.wrapping_add(i * size).cast());
        }
        assert_eq!(fixture.owned().used() as usize, blocks.len());
        assert!(fixture.owned().pop().is_null());
    }

    #[test]
    fn local_frees_come_back_after_a_collect() {
        let fixture = Fixture::new(64);
        let owned = fixture.owned();
        let blocks = fixture.exhaust();
        // SAFETY: the blocks were popped from this slab and are no longer used.
        unsafe {
            owned.push_local(blocks[3]);
            owned.push_local(blocks[9]);
        }
        assert!(owned.collect(false));
        assert_eq!((owned.pop(), owned.pop()), (blocks[9], blocks[3]));
        assert!(owned.pop().is_null());
    }

    #[test]
    fn remote_frees_are_counted_when_collected() {
        let fixture = Fixture::new(64);
        let owned = fixture.owned();
        let blocks = fixture.exhaust();
        let used = owned.used();
        // SAFETY: as above.
        unsafe {
            assert!(!fixture.slab.push_remote(blocks[0]));
            assert!(!fixture.slab.push_remote(blocks[1]));
        }
        assert_eq!(owned.used(), used, "pending remote frees still count as used");
        assert!(owned.collect(false));
        assert_eq!(owned.used(), used - 2);
        assert_eq!((owned.pop(), owned.pop()), (blocks[1], blocks[0]));
    }

    #[test]
    fn the_first_remote_free_into_a_full_slab_notifies_exactly_once() {
        let fixture = Fixture::new(64);
        let owned = fixture.owned();
        let blocks = fixture.exhaust();
        assert!(owned.try_mark_full());
        // SAFETY: as above.
        unsafe {
            assert!(fixture.slab.push_remote(blocks[0]), "the first free must notify");
            assert!(!fixture.slab.push_remote(blocks[1]), "later frees must not");
        }
        assert!(owned.is_queued());
        // Collecting keeps the notification pending until it is acknowledged.
        assert!(owned.collect(false));
        assert!(owned.is_queued());
        assert!(!owned.try_mark_full(), "a pending notification blocks re-marking");
    }

    #[test]
    fn acknowledging_a_notification_allows_marking_full_again() {
        let fixture = Fixture::new(64);
        let owned = fixture.owned();
        let blocks = fixture.exhaust();
        assert!(owned.try_mark_full());
        // SAFETY: as above.
        unsafe { assert!(fixture.slab.push_remote(blocks[0])) };
        assert!(owned.collect(true));
        assert!(!owned.is_queued());
        assert!(owned.pop() == blocks[0]);
        assert!(owned.try_mark_full());
    }

    #[test]
    fn marking_full_fails_while_remote_blocks_are_pending() {
        let fixture = Fixture::new(64);
        let blocks = fixture.exhaust();
        // SAFETY: as above.
        unsafe { assert!(!fixture.slab.push_remote(blocks[0])) };
        assert!(!fixture.owned().try_mark_full());
    }

    #[test]
    fn clearing_the_full_mark_stops_notifications_but_not_a_sent_one() {
        let fixture = Fixture::new(64);
        let owned = fixture.owned();
        let blocks = fixture.exhaust();
        assert!(owned.try_mark_full());
        owned.clear_full();
        // SAFETY: as above.
        unsafe { assert!(!fixture.slab.push_remote(blocks[0])) };
        assert!(!owned.is_queued());

        assert!(owned.collect(false));
        assert!(owned.try_mark_full());
        // SAFETY: as above.
        unsafe { assert!(fixture.slab.push_remote(blocks[1])) };
        owned.clear_full();
        assert!(owned.is_queued(), "clearing the mark must not lose a sent notification");
    }

    #[test]
    fn enqueued_slabs_come_out_most_recent_first() {
        let (a, b) = (Slab::new(), Slab::new());
        let stack = AtomicPtr::new(ptr::null_mut());
        a.enqueue(&stack);
        b.enqueue(&stack);
        let first = Slab::take_enqueued(&stack);
        assert!(ptr::eq(first, &b));
        // SAFETY: the slabs outlive the test.
        let second = unsafe { &*first }.next_enqueued();
        assert!(ptr::eq(second, &a));
        // SAFETY: as above.
        assert!(unsafe { &*second }.next_enqueued().is_null());
        assert!(Slab::take_enqueued(&stack).is_null());
    }

    #[test]
    fn an_all_zero_descriptor_is_an_unused_one() {
        // SAFETY: this is the claim under test, and Miri checks that nothing here reads invalid
        // state.
        let slab: Slab = unsafe { core::mem::MaybeUninit::zeroed().assume_init() };
        let fresh = Slab::new();
        // SAFETY: single-threaded test.
        let (zeroed, fresh) = unsafe { (slab.owned(), fresh.owned()) };
        assert_eq!(zeroed.used(), fresh.used());
        assert_eq!(zeroed.list(), List::None);
        assert!(zeroed.pop().is_null());
        assert!(slab.shared.remote.load(Ordering::Relaxed).is_null());
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use loom::{sync::Arc, thread};

    use super::*;
    use crate::testing::Buffer;

    /// A slab with every block handed out, plus the blocks.
    struct Exhausted {
        slab: Slab,
        blocks: std::vec::Vec<*mut Block>,
        _memory: Buffer,
    }

    // SAFETY: the blocks are only ever passed to slab operations, never dereferenced here, and
    // each test gives the owner role to exactly one thread.
    unsafe impl Send for Exhausted {}
    // SAFETY: as above.
    unsafe impl Sync for Exhausted {}

    impl Exhausted {
        fn new() -> Arc<Self> {
            let class = Class::for_size(8192);
            let memory = Buffer::new(class.slab_chunks() << crate::chunks::CHUNK_SHIFT);
            let slab = Slab::new();
            // SAFETY: nothing else uses the fresh descriptor yet.
            let owned = unsafe { slab.owned() };
            // SAFETY: the buffer is slab sized and aligned, and private to this slab.
            unsafe { owned.init(1, class, memory.as_ptr()) };
            let mut blocks = std::vec::Vec::new();
            loop {
                let block = owned.pop();
                if !block.is_null() {
                    blocks.push(block);
                } else if !owned.extend() {
                    break;
                }
            }
            Arc::new(Self { slab, blocks, _memory: memory })
        }

        /// The owner-only half. Each test calls this from the one thread that plays the owner.
        const fn owner(&self) -> Owned<'_> {
            // SAFETY: see the contract above.
            unsafe { self.slab.owned() }
        }

        /// Frees block `index` from another thread and returns whether that push must notify.
        fn push(&self, index: usize) -> bool {
            // SAFETY: the block was handed out by `new` and each test frees it once.
            unsafe { self.slab.push_remote(self.blocks[index]) }
        }
    }

    #[test]
    fn marking_full_and_a_racing_remote_free_never_lose_the_notification() {
        loom::model(|| {
            let state = Exhausted::new();
            let pusher = {
                let state = state.clone();
                thread::spawn(move || state.push(0))
            };
            let marked = state.owner().try_mark_full();
            let notified = pusher.join().unwrap();
            // Either the owner saw the pending block and refused to mark the slab full, or the
            // pusher saw the mark and promised to notify. Both losing is the lost wakeup.
            assert!(!marked || notified, "marked full but nobody will notify the owner");
            assert_eq!(marked, notified);
        });
    }

    #[test]
    fn clearing_the_mark_and_a_racing_remote_free_stay_consistent() {
        loom::model(|| {
            let state = Exhausted::new();
            assert!(state.owner().try_mark_full());
            let pusher = {
                let state = state.clone();
                thread::spawn(move || state.push(0))
            };
            state.owner().clear_full();
            let notified = pusher.join().unwrap();
            // A notification that was sent must still be recorded after the owner cleared its mark.
            assert_eq!(state.owner().is_queued(), notified);
            // The pushed block is never lost either way.
            assert!(state.owner().collect(true));
            assert_eq!(state.owner().pop(), state.blocks[0]);
        });
    }

    #[test]
    fn concurrent_remote_frees_are_all_collected_with_one_notification() {
        loom::model(|| {
            let state = Exhausted::new();
            assert!(state.owner().try_mark_full());
            let pushers: std::vec::Vec<_> = (0..2)
                .map(|index| {
                    let state = state.clone();
                    thread::spawn(move || usize::from(state.push(index)))
                })
                .collect();
            let notifications: usize = pushers.into_iter().map(|p| p.join().unwrap()).sum();
            assert_eq!(notifications, 1);
            assert!(state.owner().collect(true));
            assert_eq!(state.owner().used() as usize, state.blocks.len() - 2);
        });
    }
}
