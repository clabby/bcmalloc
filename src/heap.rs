//! Per-thread heaps, which hold the allocation fast path and everything behind it.
//!
//! A [`Heap`] is one thread's private view of the region. It owns a set of slabs grouped by size
//! class, and it serves most requests by popping a block off the front slab of the right class with
//! no atomic operations at all.
//!
//! ```text
//!   size class        active slabs (have room)                full slabs
//!   +---------+       +------+    +------+    +------+        +------+    +------+
//!   | 16 B    |-----> | slab |--->| slab |--->| slab |        | slab |--->| slab |
//!   +---------+       +------+    +------+    +------+        +------+    +------+
//!   | 32 B    |-----> nil           ^ pop here                  ^ parked until a block comes
//!   +---------+                     |                           | back and the owner is told
//!   | ...     |                     front slab                  | through the unfull stack
//!   +---------+
//! ```
//!
//! # Allocating
//!
//! 1. Pop the free list of the front active slab. This is the whole fast path.
//! 2. If it is empty, fold in the local and remote free lists of the slab, or thread a few more
//!    never-used blocks onto it.
//! 3. If the slab has nothing left, park it on the full list and try the next one. A slab that gets
//!    a block back joins the active list at the back, so that allocation keeps draining the front
//!    slab and a returning slab has collected many blocks by the time it is reached.
//! 4. If the class has no active slab, adopt slabs that other threads have freed into, or take a
//!    fresh one from the chunk allocator.
//!
//! # Freeing
//!
//! The slab holding a block is found by arithmetic. The offset of the block into the region selects
//! a chunk, and masking that chunk index with the slab size selects the first chunk of the slab,
//! which is where its descriptor lives. The layout supplied by the caller gives the size class, so
//! no header is ever read from the block.
//!
//! ```text
//!                     +- owner is this heap ----> push on the slab's local_free   (no atomics)
//!   free(ptr, layout) |
//!                     +- owner is another heap -> push on the slab's remote list  (one CAS)
//!                                                    |
//!                                                    +-> if the slab was marked full, push the
//!                                                        slab on the owner's unfull stack
//! ```
//!
//! A slab whose last block is freed goes back to the chunk allocator. Each class keeps one empty
//! slab, so a block that is allocated and freed in a loop never bounces a slab in and out of the
//! chunk allocator.
//!
//! # Heap lifetime
//!
//! A heap is a claimable object and not a property of a thread. When a thread exits, its heap is
//! released with all its slabs intact and the next thread to start picks it up. Memory held by a
//! departed thread never needs to be handed over, and frees that arrive for it keep landing on the
//! same slabs.

use core::{alloc::Layout, cell::Cell, fmt, marker::PhantomData, ptr, ptr::NonNull};

use crate::{
    cache::CacheLine,
    chunks::CHUNK_SHIFT,
    region::Region,
    size_class::{CLASSES, Class, Request, classify},
    slab::{Block, List, Owned, Slab},
    sync::{AtomicBool, AtomicPtr, Ordering},
};

/// The slab lists of a size class. The active list is tracked from both ends.
struct Lists {
    active: Cell<*const Slab>,
    active_last: Cell<*const Slab>,
    full: Cell<*const Slab>,
}

/// Blocks that a heap has freed into one slab of another heap and has not handed over yet.
///
/// They are linked from `head` to `tail` through their first words. Handing them over takes one
/// atomic operation for the whole chain, where freeing them one by one takes one each, and the
/// stores that link them can all be in flight at once because nothing waits for them.
#[derive(Clone, Copy)]
struct Batch {
    slab: *const Slab,
    head: *mut Block,
    tail: *mut Block,
    count: u32,
}

impl Batch {
    const EMPTY: Self =
        Self { slab: ptr::null(), head: ptr::null_mut(), tail: ptr::null_mut(), count: 0 };
}

/// What other threads touch in a heap slot, kept on a line of its own.
struct Shared {
    _line: [CacheLine; 0],
    claimed: AtomicBool,
    unfull: AtomicPtr<Slab>,
}

/// What only the claimant touches in a heap slot.
struct Private {
    _line: [CacheLine; 0],
    id: Cell<u32>,
    cursor: Cell<usize>,
    /// The region this slot was claimed from, and its slab table, so that the fast paths reach
    /// both without loading the region first. They are set at every claim and valid while the
    /// claim lasts.
    region: Cell<*const Region<'static>>,
    base: Cell<*mut u8>,
    slabs: Cell<*const Slab>,
    /// For each class, blocks of the front active slab that the allocation fast path hands out.
    hot: [Cell<*mut Block>; CLASSES],
    /// For each class, the blocks of one foreign slab that this heap has freed lately.
    remote: [Cell<Batch>; CLASSES],
    classes: [Lists; CLASSES],
}

/// The state of one heap slot in a region.
///
/// Everything but `claimed` and `unfull` belongs to whichever thread has claimed the slot. An
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
            .field("unfull", &self.shared.unfull)
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
                unfull: AtomicPtr::new(ptr::null_mut()),
            },
            private: Private {
                _line: [],
                id: Cell::new(0),
                cursor: Cell::new(0),
                region: Cell::new(ptr::null()),
                base: Cell::new(ptr::null_mut()),
                slabs: Cell::new(ptr::null()),
                hot: [const { Cell::new(ptr::null_mut()) }; CLASSES],
                remote: [const { Cell::new(Batch::EMPTY) }; CLASSES],
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
            self.private.id.set(id);
            self.private.region.set(ptr::from_ref(region).cast());
            self.private.base.set(region.chunk_ptr(0));
            self.private.slabs.set(region.slabs_ptr());
        }
        won
    }

    /// Hands over everything this heap still holds for other heaps and gives the slot up for
    /// another thread to claim.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    pub(crate) unsafe fn give_up(&self) {
        // SAFETY: forwarded.
        unsafe { self.hand_over_all() };
        self.release_claim();
    }

    /// Gives the slot up for another thread to claim.
    pub(crate) fn release_claim(&self) {
        self.shared.claimed.store(false, Ordering::Release);
    }

    pub(crate) fn is_claimed(&self) -> bool {
        self.shared.claimed.load(Ordering::Relaxed)
    }

    /// The stack of slabs that other threads have freed into while they were full.
    pub(crate) const fn unfull(&self) -> &AtomicPtr<Slab> {
        &self.shared.unfull
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

    /// Allocates memory for the layout, returning null if the region is out of room or the layout
    /// is not supported. Common requests take a table lookup and a pop from the hot list.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(always)]
    pub(crate) unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Class::is_table_request(layout) {
            let hot = &self.private.hot[Class::from_table(layout.size()).index()];
            let block = hot.get();
            if !block.is_null() {
                // SAFETY: every block on a hot list is unused slab memory whose first word is a
                // link.
                hot.set(unsafe { (*block).next });
                return block.cast();
            }
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_general(layout) }
    }

    /// Allocates anything that the fast path in [`alloc`] does not take.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline(never)]
    unsafe fn alloc_general(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        let Some(request) = classify(layout) else { return ptr::null_mut() };
        let block = self.try_alloc(region, request);
        if !block.is_null() {
            return block;
        }
        // The region looks full, but this heap may be sitting on empty slabs. Give them back and
        // retry.
        // SAFETY: the caller holds the claim.
        unsafe { self.flush(region) };
        self.try_alloc(region, request)
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

    /// Checks every structural invariant of this heap and panics on a violation.
    ///
    /// It walks all lists, so it is for tests. Each slab's counters must match its lists, no block
    /// may appear twice anywhere, the hot lists must hold blocks of their class's front active
    /// slab, and the flags in the remote word must agree with the list the slab is on.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot and must not run it concurrently with remote
    /// frees, which would change the remote lists under the walk.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) unsafe fn validate(&self) {
        use std::collections::BTreeSet;

        let mut seen = BTreeSet::new();
        let mut claim = |block: *mut Block, what: &str| {
            assert!(seen.insert(block.addr()), "block {block:p} appears twice, last on {what}");
        };
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            let size = class.block_size();
            let lists = &self.private.classes[index];
            let mut slabs = [(List::Active, lists.active.get()), (List::Full, lists.full.get())];
            let mut last = ptr::null();
            for (list, head) in &mut slabs {
                let mut prev: *const Slab = ptr::null();
                let mut current = *head;
                // SAFETY: slabs on our lists are ours.
                while let Some(slab) = unsafe { current.as_ref() } {
                    // SAFETY: as above.
                    let owned = unsafe { slab.owned() };
                    assert_eq!(owned.list(), *list, "slab is on the wrong list");
                    assert_eq!(owned.class(), class, "slab is on the wrong class");
                    assert!(ptr::eq(owned.prev(), prev), "broken back link");
                    assert_eq!(slab.owner(), self.private.id.get(), "slab owned by another heap");
                    let state = owned.inspect();
                    match list {
                        List::Active => assert!(!state.is_marked_full(), "active slab marked full"),
                        _ => assert!(
                            state.is_marked_full() || state.is_queued(),
                            "full slab nobody will notify"
                        ),
                    }
                    assert!(state.bumped <= state.capacity);
                    assert!(
                        state.free_len + state.local_count <= state.bumped,
                        "more free blocks than blocks that were ever handed out"
                    );
                    let in_slab = |block: *mut Block| {
                        let offset = block.addr().wrapping_sub(state.start.addr());
                        offset % size == 0 && offset / size < state.bumped as usize
                    };
                    let mut walk = |mut block: *mut Block, expected: Option<u32>, what: &str| {
                        let mut count = 0;
                        while !block.is_null() {
                            assert!(in_slab(block), "{what} holds a block outside its slab");
                            claim(block, what);
                            count += 1;
                            // SAFETY: the block is unused slab memory whose first word is a link.
                            block = unsafe { (*block).next };
                        }
                        if let Some(expected) = expected {
                            assert_eq!(count, expected, "{what} length disagrees with its counter");
                        }
                        count
                    };
                    walk(state.free, Some(state.free_len), "the free list");
                    walk(state.local_free, Some(state.local_count), "the local free list");
                    let pending = walk(state.remote, None, "the remote list");
                    assert!(
                        pending <= state.bumped - state.free_len - state.local_count,
                        "pending remote blocks must count as used"
                    );
                    prev = current;
                    current = owned.next();
                    last = prev;
                }
                if *list == List::Active {
                    assert!(ptr::eq(lists.active_last.get(), prev), "wrong last active slab");
                    last = ptr::null();
                }
            }
            let _ = last;
            let held = self.private.remote[index].get();
            if !held.slab.is_null() {
                assert!(held.count >= 1 && held.count < class.batch_limit().max(2));
                assert_ne!(
                    // SAFETY: the slab is live because the batch's blocks are in use, and the
                    // owner id is never written while it is.
                    unsafe { (*held.slab).owner() },
                    self.private.id.get(),
                    "a batch holds blocks of another heap's slab"
                );
                // The slab belongs to another heap, whose private fields may be changing, so its
                // extent comes from the position of its descriptor instead.
                let index =
                    (held.slab.addr() - self.private.slabs.get().addr()) / size_of::<Slab>();
                let start = self.private.base.get().wrapping_add(index << CHUNK_SHIFT);
                let mut block = held.head;
                for position in 0..held.count {
                    let offset = block.addr().wrapping_sub(start.addr());
                    assert!(
                        offset % size == 0 && offset / size < class.blocks_per_slab(),
                        "batched block outside its slab"
                    );
                    claim(block, "a remote batch");
                    if position + 1 == held.count {
                        assert_eq!(block, held.tail, "the batch tail is not the last block");
                    } else {
                        // SAFETY: the block is ours and its first word links the batch.
                        block = unsafe { (*block).next };
                    }
                }
            }
            let mut hot = self.private.hot[index].get();
            if !hot.is_null() {
                // SAFETY: slabs on our lists are ours.
                let front = unsafe { lists.active.get().as_ref() }
                    .expect("a hot list needs a front active slab");
                // SAFETY: as above.
                let state = unsafe { front.owned() }.inspect();
                while !hot.is_null() {
                    let offset = hot.addr().wrapping_sub(state.start.addr());
                    assert!(
                        offset % size == 0 && offset / size < state.bumped as usize,
                        "hot block outside the front active slab"
                    );
                    claim(hot, "a hot list");
                    // SAFETY: the block is unused slab memory whose first word is a link.
                    hot = unsafe { (*hot).next };
                }
            }
        }
    }

    /// Frees a block of another heap's slab. The block joins the batch for its class, which is
    /// handed over when it is full or when the next remote free of that class is for another slab.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the block must be a live block of `slab` that is freed
    /// exactly once. The slab must belong to another heap.
    #[inline(never)]
    unsafe fn free_remote(&self, slab: &Slab, block: *mut Block, class: Class) {
        let entry = &self.private.remote[class.index()];
        let mut batch = entry.get();
        if ptr::eq(batch.slab, slab) {
            // SAFETY: the block is ours until the batch is handed over.
            unsafe { (*block).next = batch.head };
            batch.head = block;
        } else {
            // SAFETY: the claim keeps the region alive, and the batch holds blocks of one slab.
            unsafe { self.hand_over(batch) };
            batch = Batch { slab, head: block, tail: block, count: 0 };
        }
        batch.count += 1;
        if batch.count >= class.batch_limit() {
            // SAFETY: as above.
            unsafe { self.hand_over(batch) };
            batch = Batch::EMPTY;
        }
        entry.set(batch);
    }

    /// Pushes a batch onto its slab's remote list. An empty batch needs nothing.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, and the batch must hold blocks of one live slab that nobody
    /// else is freeing.
    unsafe fn hand_over(&self, batch: Batch) {
        // SAFETY: a batch with blocks has a slab, which cannot be recycled while it has blocks
        // that are still in use, and the caller holds the claim that keeps the region alive.
        if let Some(slab) = unsafe { batch.slab.as_ref() } {
            // SAFETY: the batch holds blocks of the slab, linked head to tail.
            unsafe { self.region().free_remote_chain(slab, batch.head, batch.tail) };
        }
    }

    /// Hands over every batch.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn hand_over_all(&self) {
        for entry in &self.private.remote {
            let batch = entry.replace(Batch::EMPTY);
            // SAFETY: forwarded.
            unsafe { self.hand_over(batch) };
        }
    }

    /// The region this slot is claimed from.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    #[inline(always)]
    unsafe fn region(&self) -> &Region<'_> {
        // SAFETY: the claim was made on this region, which outlives every claim on its slots.
        unsafe { &*self.private.region.get() }
    }

    fn try_alloc(&self, region: &Region<'_>, request: Request) -> *mut u8 {
        match request {
            // SAFETY: the caller of the public entry point holds the claim.
            Request::Small(class) => unsafe { self.alloc_small(region, class) },
            Request::Huge { chunks } => self.alloc_huge(region, chunks),
        }
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
            // Table classes have one-chunk slabs, so the chunk index is the slab index.
            let index = (ptr.as_ptr().addr() - self.private.base.get().addr()) >> CHUNK_SHIFT;
            // SAFETY: the block lies in the region, which has a descriptor for every chunk.
            let slab = unsafe { &*self.private.slabs.get().add(index) };
            if slab.owner() == self.private.id.get() {
                // SAFETY: we own the slab and hold the claim, and the caller vouches for the
                // block.
                unsafe { self.free_local(slab.owned(), ptr.as_ptr().cast()) };
            } else {
                // SAFETY: the caller vouches for the block, and the claim keeps the region alive.
                unsafe {
                    self.free_remote(slab, ptr.as_ptr().cast(), Class::from_table(layout.size()));
                }
            }
        } else {
            // SAFETY: forwarded.
            unsafe { self.dealloc_general(ptr, layout) };
        }
    }

    #[inline(never)]
    unsafe fn dealloc_general(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the claim keeps the region alive.
        let region = unsafe { self.region() };
        match classify(layout) {
            Some(Request::Small(class)) => {
                let slab = region.slab_of(ptr, class);
                if slab.owner() == self.private.id.get() {
                    // SAFETY: we own the slab and hold the claim, and the caller vouches for the
                    // block.
                    unsafe { self.free_local(slab.owned(), ptr.as_ptr().cast()) };
                } else {
                    // SAFETY: the caller vouches for the block, and the claim keeps the region
                    // alive.
                    unsafe { self.free_remote(slab, ptr.as_ptr().cast(), class) };
                }
            }
            // SAFETY: forwarded.
            _ => unsafe { region.dealloc(ptr, layout) },
        }
    }

    /// Allocates one block of `class`, returning null if the region is out of room.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim on this slot.
    #[inline]
    pub(crate) unsafe fn alloc_small(&self, region: &Region<'_>, class: Class) -> *mut u8 {
        let hot = &self.private.hot[class.index()];
        let block = hot.get();
        if !block.is_null() {
            // SAFETY: every block on a hot list is unused slab memory whose first word is a link.
            hot.set(unsafe { (*block).next });
            return block.cast();
        }
        // SAFETY: forwarded.
        unsafe { self.alloc_slow(region, class) }
    }

    /// Everything the fast path cannot do. Refills the hot list from the front active slab and
    /// hands out its first block.
    #[cold]
    unsafe fn alloc_slow(&self, region: &Region<'_>, class: Class) -> *mut u8 {
        region.purge_if_due();
        loop {
            if !self.shared.unfull.load(Ordering::Relaxed).is_null() {
                // SAFETY: the caller holds the claim.
                unsafe { self.drain_notified(region) };
            }
            // SAFETY: slabs on our lists are ours.
            let Some(slab) = (unsafe { self.lists(class).active.get().as_ref() }) else {
                // SAFETY: the caller holds the claim.
                if unsafe { self.add_slab(region, class) } {
                    continue;
                }
                return ptr::null_mut();
            };
            // SAFETY: as above.
            let slab = unsafe { slab.owned() };
            if slab.collect(false) || slab.extend() {
                let block = slab.take_free();
                // SAFETY: the slab has free blocks, so the list is not empty, and its blocks are
                // unused slab memory whose first word is a link.
                self.private.hot[class.index()].set(unsafe { (*block).next });
                return block.cast();
            }
            self.park_full(slab);
        }
    }

    /// Takes a fresh slab of `class` from the chunk allocator and puts it at the front of the
    /// active list. Returns false if the region has no room.
    unsafe fn add_slab(&self, region: &Region<'_>, class: Class) -> bool {
        let chunks = class.slab_chunks();
        let Some(first) = region.alloc_chunks(chunks, chunks, self.private.cursor.get()) else {
            return false;
        };
        let slab = region.slab(first);
        // SAFETY: the chunks were just allocated to us, so nobody else uses this descriptor.
        let owned = unsafe { slab.owned() };
        // SAFETY: the chunks are a slab-sized, slab-aligned run that is ours alone.
        unsafe { owned.init(self.private.id.get(), class, region.chunk_ptr(first)) };
        self.push_front(owned, List::Active);
        if chunks == 1 {
            self.private.cursor.set(first / 64);
        }
        true
    }

    /// Moves a slab that has no free block off the allocation path. If a remote free slips in
    /// while we do, its blocks are collected and the slab ends up on the active list.
    fn park_full(&self, slab: Owned<'_>) {
        loop {
            // Either the full mark sticks and the next remote free will notify us, or a pending
            // notification already promises that someone will.
            if slab.try_mark_full() || slab.is_queued() {
                self.move_to(slab, List::Full);
                return;
            }
            // A remote free got in first, so its blocks are ours to take.
            if slab.collect(false) {
                if slab.list() == List::Full {
                    self.move_to(slab, List::Active);
                }
                return;
            }
        }
    }

    /// Processes every slab that remote frees have notified us about.
    unsafe fn drain_notified(&self, region: &Region<'_>) {
        let mut next = Slab::take_enqueued(&self.shared.unfull);
        // SAFETY: a queued slab cannot be recycled before it is popped, so the pointer is valid.
        while let Some(slab) = unsafe { next.as_ref() } {
            next = slab.next_enqueued();
            // SAFETY: a slab that notified us is on our lists.
            self.revisit(region, unsafe { slab.owned() });
        }
    }

    /// Acknowledges a notification by taking the blocks that arrived and putting the slab where its
    /// contents say it belongs.
    fn revisit(&self, region: &Region<'_>, slab: Owned<'_>) {
        let has_free = slab.collect(true);
        if slab.used() == 0 {
            self.retire(region, slab);
        } else if slab.list() == List::Full {
            if has_free {
                self.move_to(slab, List::Active);
            } else {
                self.park_full(slab);
            }
        }
    }

    /// Frees `block`, which belongs to `slab`, from the owning thread.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim, `slab` must be owned by this heap, and `block` must be an
    /// allocated block of `slab`.
    #[inline(always)]
    pub(crate) unsafe fn free_local(&self, slab: Owned<'_>, block: *mut Block) {
        let was_full = slab.list() == List::Full;
        // SAFETY: forwarded.
        unsafe { slab.push_local(block) };
        if was_full || slab.used() == 0 {
            self.free_local_rare(slab, was_full);
        }
    }

    /// The part of [`free_local`](Self::free_local) that changes which list a slab is on or gives
    /// it back.
    #[cold]
    #[inline(never)]
    fn free_local_rare(&self, slab: Owned<'_>, was_full: bool) {
        if was_full {
            // It has a free block again, so it belongs where allocation looks. A full slab holds
            // every block but this one, so it cannot be empty.
            slab.clear_full();
            self.move_to(slab, List::Active);
        } else {
            // SAFETY: the caller of free_local holds the claim.
            self.retire(unsafe { self.region() }, slab);
        }
    }

    /// Handles a slab with no live blocks. Its chunks are returned unless it is the only active
    /// slab of its class, in which case it is kept for the next allocation.
    fn retire(&self, region: &Region<'_>, slab: Owned<'_>) {
        debug_assert_eq!(slab.used(), 0);
        let sole_active =
            slab.list() == List::Active && slab.prev().is_null() && slab.next().is_null();
        // A pending notification still points at the slab, so it must stay until processed.
        if !sole_active && !slab.is_queued() {
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

    /// Hands every empty slab back to the chunk allocator, folding in pending frees first.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    pub(crate) unsafe fn flush(&self, region: &Region<'_>) {
        // SAFETY: forwarded.
        unsafe { self.hand_over_all() };
        // SAFETY: forwarded.
        unsafe { self.give_back_hot(region) };
        // SAFETY: forwarded.
        unsafe { self.drain_notified(region) };
        for index in 0..CLASSES {
            let class = Class::from_index(index);
            for list in [List::Active, List::Full] {
                let mut next = self.head(class, list).get();
                // SAFETY: slabs on our lists are ours, and `next` is read before the slab can move.
                while let Some(slab) = unsafe { next.as_ref() } {
                    // SAFETY: as above.
                    let slab = unsafe { slab.owned() };
                    next = slab.next();
                    self.sweep(region, slab);
                }
            }
        }
    }

    /// Returns the blocks on the hot lists to their slabs, which cannot be released while those
    /// blocks count as in use.
    ///
    /// # Safety
    ///
    /// The caller must hold the claim.
    unsafe fn give_back_hot(&self, region: &Region<'_>) {
        for (index, hot) in self.private.hot.iter().enumerate() {
            let class = Class::from_index(index);
            let mut block = hot.replace(ptr::null_mut());
            let Some(first) = NonNull::new(block) else { continue };
            // SAFETY: a hot list holds blocks of the front active slab, which is ours.
            let slab = unsafe { region.slab_of(first.cast(), class).owned() };
            while !block.is_null() {
                // SAFETY: the block is unused slab memory whose first word is a link.
                let next = unsafe { (*block).next };
                // SAFETY: the block belongs to this slab and nobody is using it.
                unsafe { slab.push_local(block) };
                block = next;
            }
        }
    }

    /// Folds in a slab's pending frees, releases it if nothing is live, and otherwise makes sure it
    /// is on the right list.
    fn sweep(&self, region: &Region<'_>, slab: Owned<'_>) {
        let has_free = slab.collect(false);
        if slab.used() == 0 && !slab.is_queued() {
            self.release(region, slab);
        } else if has_free && slab.list() == List::Full {
            slab.clear_full();
            self.move_to(slab, List::Active);
        }
    }

    /// Allocates a run of `chunks` whole chunks, returning null if there is no room.
    pub(crate) fn alloc_huge(&self, region: &Region<'_>, chunks: usize) -> *mut u8 {
        region.alloc_chunks(chunks, 1, 0).map_or(ptr::null_mut(), |first| region.chunk_ptr(first))
    }

    /// Moves a slab to the full list, or to the back of the active list.
    ///
    /// A slab that becomes active again usually has a single free block. Allocation keeps using
    /// the slab at the front until it runs dry, and by the time the returning slab gets its turn
    /// it has collected many more blocks. At the front it would be drained and parked again after
    /// nearly every free.
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

    /// Checks the heap's structural invariants. See [`HeapData`::validate].
    #[cfg(any(test, feature = "testing"))]
    pub fn validate(&self) {
        // SAFETY: this handle holds the claim. Tests do not free remotely while it runs.
        unsafe { self.data.validate() };
    }

    /// Gives up this handle without releasing the claim, which the caller takes over. The slot
    /// stays claimed until [`release_claim`](HeapData::release_claim) is called on it.
    #[cfg(all(feature = "std", not(loom)))]
    pub(crate) const fn leak(self) -> &'r HeapData {
        let data = self.data;
        core::mem::forget(self);
        data
    }

    /// Returns every empty slab of this heap to the region, so that other heaps (and large
    /// allocations) can use the memory. [`Region::trim`] does this for every idle heap.
    pub fn flush(&self) {
        // SAFETY: this handle holds the claim.
        unsafe { self.data.flush(self.region) };
    }
}

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
    fn a_class_keeps_one_empty_slab_and_flush_returns_it() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(64, 8);
        // 64-byte blocks are 1024 to a slab, so this spans three slabs.
        let blocks = alloc_many(&heap, layout, 3000);
        assert_eq!(used(&region), 3 * CHUNK_SIZE);

        free_all(&heap, layout, blocks);
        assert_eq!(used(&region), CHUNK_SIZE, "all but one empty slab should be released");
        heap.flush();
        assert_eq!(used(&region), 0);
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
    fn remote_frees_are_reused_by_the_owner() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        let layout = layout(100, 8);
        let blocks = alloc_many(&owner, layout, 500);
        let baseline = used(&region);

        free_all(&other, layout, blocks);
        other.flush();
        let again = alloc_many(&owner, layout, 500);
        assert_eq!(used(&region), baseline, "the owner must recycle what the other heap freed");
        free_all(&owner, layout, again);
    }

    #[test]
    fn a_remote_free_into_a_full_slab_makes_its_block_the_next_allocation() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        // 8 KiB blocks, so a slab holds exactly eight.
        let layout = layout(8192, 8);
        let first = alloc_many(&owner, layout, 8);
        // The ninth parks the first slab as full and opens a second, which the next seven fill.
        alloc_many(&owner, layout, 8);
        assert_eq!(used(&region), 2 * CHUNK_SIZE);

        // SAFETY: allocated by the owner with this layout and freed once.
        unsafe { other.dealloc(first[3], layout) };
        other.flush();
        // Both slabs are full now, so the owner has to learn about the notification.
        assert_eq!(owner.alloc(layout), Some(first[3]));
        assert_eq!(used(&region), 2 * CHUNK_SIZE, "no third slab should be needed");
    }

    #[test]
    fn remote_frees_are_held_back_until_a_batch_fills_moves_on_or_is_flushed() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        // 8 KiB blocks, so a slab holds eight and a batch of the class holds two.
        let layout = layout(8192, 8);
        let class = Class::for_size(8192);
        assert_eq!(class.batch_limit(), 2);
        let blocks = alloc_many(&owner, layout, 16);
        let pending = |block: NonNull<u8>| {
            // SAFETY: single-threaded test, so nothing else touches the owner's half.
            unsafe { region.slab_of(block, class).owned() }.inspect().remote
        };
        let free = |block: NonNull<u8>| {
            // SAFETY: the block is live, from this region, with this layout, and freed once.
            unsafe { other.dealloc(block, layout) };
        };
        let batched = || other.data.private.remote[class.index()].get().count;

        free(blocks[0]);
        assert_eq!(batched(), 1);
        assert!(pending(blocks[0]).is_null(), "a held back block is not visible to the owner yet");

        // The second block fills the batch, so both blocks are handed over.
        free(blocks[1]);
        assert_eq!(batched(), 0);
        assert!(!pending(blocks[0]).is_null());

        // A block of another slab makes the heap hand over what it holds for the first.
        free(blocks[2]);
        assert_eq!(batched(), 1);
        free(blocks[8]);
        assert_eq!(batched(), 1, "the block of the second slab starts a new batch");
        assert!(!pending(blocks[2]).is_null(), "the batch for the first slab was handed over");
        assert!(pending(blocks[8]).is_null());

        other.flush();
        assert_eq!(batched(), 0);
        assert!(!pending(blocks[8]).is_null());
        owner.validate();
        other.validate();
    }

    #[test]
    fn a_slab_with_a_returned_block_waits_behind_the_one_being_filled() {
        let test = TestRegion::new(128, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        // 8 KiB blocks, so a slab holds exactly eight. Two slabs are full and a third has one block
        // out.
        let layout = layout(8192, 8);
        let blocks = alloc_many(&heap, layout, 17);
        // SAFETY: allocated by this heap with this layout and freed once.
        unsafe { heap.dealloc(blocks[0], layout) };

        let third = region.chunk_index(blocks[16].as_ptr());
        let rest = alloc_many(&heap, layout, 7);
        assert!(rest.iter().all(|block| region.chunk_index(block.as_ptr()) == third));
        assert_eq!(heap.alloc(layout), Some(blocks[0]));
    }

    #[test]
    fn parking_a_slab_that_regained_blocks_leaves_it_where_allocation_looks() {
        let test = TestRegion::new(128, 2);
        let region = test.region();
        let (owner, other) = (region.heap().unwrap(), region.heap().unwrap());
        let layout = layout(8192, 8);
        let blocks = alloc_many(&owner, layout, 8);
        // SAFETY: the owner holds the claim on a slab that it allocated from.
        let slab = unsafe { region.slab_of(blocks[0], Class::for_size(8192)).owned() };
        // This is where a notification leaves a slab that had nothing free to collect. No flag
        // is set, so the next remote free is not announced.
        owner.data.move_to(slab, List::Full);

        // SAFETY: allocated by the owner with this layout and freed once.
        unsafe { other.dealloc(blocks[1], layout) };
        other.flush();
        owner.data.park_full(slab);
        assert_eq!(slab.list(), List::Active);
        assert_eq!(owner.alloc(layout), Some(blocks[1]));
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
        assert_eq!(used(&region), CHUNK_SIZE);

        // A heap that is in use is skipped, an idle one is flushed.
        region.trim();
        assert_eq!(used(&region), CHUNK_SIZE);
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
    use crate::{size_class::Class, testing::TestRegion};

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

    /// As [`forever`], for a heap that borrows the region.
    ///
    /// # Safety
    ///
    /// The same condition as [`forever`].
    unsafe fn forever_heap(heap: Heap<'_>) -> Heap<'static> {
        // SAFETY: forwarded.
        unsafe { mem::transmute(heap) }
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
    fn remote_frees_into_a_full_slab_are_never_lost() {
        loom::model(|| {
            // Two chunks, so the second slab takes the last of them and the region is full.
            let test = TestRegion::new(2, 2);
            let region = test.region();
            let (owner, remote) = (region.heap().unwrap(), region.heap().unwrap());
            let layout = layout();
            let blocks: std::vec::Vec<_> =
                (0..16).map(|_| owner.alloc(layout).expect("sixteen blocks fit")).collect();
            let class = Class::for_size(8192);
            let (x, y) = (blocks[2], blocks[5]);
            let (x_send, y_send) = (SendPtr(x), SendPtr(y));

            // SAFETY: the freeing thread is joined below, before the region goes away.
            let remote = unsafe { forever_heap(remote) };
            let freer = thread::spawn(move || {
                // SAFETY: both blocks are live, allocated with this layout, and freed once.
                unsafe {
                    remote.dealloc(x_send.0, layout);
                    remote.dealloc(y_send.0, layout);
                }
                remote
            });
            // This races with the frees. Whatever it finds must be one of the two freed blocks.
            // SAFETY: this thread holds the owner heap's claim.
            let mut got = std::vec![unsafe { owner.data.alloc_small(&region, class) }];
            drop(freer.join().unwrap());

            // Both frees have completed, so both blocks must be recoverable without any flush.
            loop {
                // SAFETY: as above.
                let block = unsafe { owner.data.alloc_small(&region, class) };
                if block.is_null() {
                    break;
                }
                got.push(block);
            }
            let mut found: std::vec::Vec<_> = got.into_iter().filter(|p| !p.is_null()).collect();
            found.sort();
            let mut expected = std::vec![x.as_ptr(), y.as_ptr()];
            expected.sort();
            assert_eq!(found, expected, "a freed block went missing or appeared twice");
        });
    }
}
