//! A slab, which is a run of chunks carved into equal blocks of one size class.
//!
//! Every slab has a descriptor, kept outside the slab so that user memory is never metadata. Only
//! the heap that owns the slab reads or writes it. Other threads that free a block of the slab send
//! the block to the owning heap instead, as the [heap module](crate::heap) explains, so the
//! descriptor needs no atomic operations at all.
//!
//! ```text
//!   slab memory, one size class (64-byte blocks here)
//!   +-------+-------+-------+-------+-------+-------+-------+-------+--------------+
//!   | block | block | block | block | block | block | block | block |  never used  |
//!   +-------+-------+-------+-------+-------+-------+-------+-------+--------------+
//!   0                                             bumped --^                capacity
//!
//!   descriptor, owner only
//!   +----------------------------------------+
//!   | free -> block -> block -> block -> nil |
//!   | free_len, bumped, capacity, class      |
//!   | start, next, prev, list                |
//!   +----------------------------------------+
//! ```
//!
//! # Free blocks
//!
//! `free` is a list of blocks that came back to the slab, threaded through their first words, and
//! the slab keeps its length so it never walks it to count. Blocks that have never been handed out
//! are on no list. The `bumped` frontier marks how many blocks have been handed out so far,
//! and refills take fresh blocks by arithmetic, so a new slab touches none of its memory until the
//! program does.
//!
//! A slab counts every block that is not on its free list and below the frontier as in use. That
//! includes blocks sitting in a heap's bins or in a message on the way back, so a slab is only
//! released when every block has really come home.

use core::{cell::Cell, ptr};

use crate::{cache::CacheLine, size_class::Class};

/// A free block, seen as a link in an intrusive list. Blocks are at least 16 bytes.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct Block {
    pub(crate) next: *mut Self,
}

/// Which of the owner's slab lists a slab is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum List {
    /// On neither list. This is an unused descriptor or a slab in the middle of a move.
    None = 0,
    /// Has free blocks or may have, and refills draw from here.
    Active = 1,
    /// Every block is in use.
    Full = 2,
}

/// The descriptor of one slab. See the [module documentation](self).
///
/// An all-zero descriptor is a valid, unused one, which is what lets the region hand out
/// descriptors straight from zeroed memory.
pub(crate) struct Slab {
    _line: [CacheLine; 0],
    free: Cell<*mut Block>,
    next: Cell<*const Self>,
    prev: Cell<*const Self>,
    start: Cell<*mut u8>,
    free_len: Cell<u32>,
    bumped: Cell<u32>,
    capacity: Cell<u32>,
    class: Cell<u8>,
    list: Cell<List>,
}

impl core::fmt::Debug for Slab {
    /// Formats nothing of the state, which belongs to the owning heap and may be changing.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slab").finish_non_exhaustive()
    }
}

// SAFETY: a descriptor is reachable for reading or writing only through [`Owned`], whose
// constructor is unsafe and requires that the caller owns the slab.
unsafe impl Sync for Slab {}

impl Slab {
    /// An unused descriptor.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const fn new() -> Self {
        Self {
            _line: [],
            free: Cell::new(ptr::null_mut()),
            next: Cell::new(ptr::null()),
            prev: Cell::new(ptr::null()),
            start: Cell::new(ptr::null_mut()),
            free_len: Cell::new(0),
            bumped: Cell::new(0),
            capacity: Cell::new(0),
            class: Cell::new(0),
            list: Cell::new(List::None),
        }
    }

    /// Gives access to the descriptor.
    ///
    /// # Safety
    ///
    /// The caller must be the only thread using the descriptor. That means it holds the heap that
    /// owns this slab, or it is initialising a descriptor whose chunks it has just claimed.
    pub(crate) const unsafe fn owned(&self) -> Owned<'_> {
        Owned(self)
    }
}

/// What [`Owned::inspect`] reports.
#[cfg(any(test, feature = "testing"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inspection {
    pub(crate) free: *mut Block,
    pub(crate) free_len: u32,
    pub(crate) bumped: u32,
    pub(crate) capacity: u32,
    pub(crate) start: *mut u8,
}

/// A descriptor in the hands of its owner, obtained from [`Slab::owned`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owned<'a>(&'a Slab);

impl<'a> Owned<'a> {
    /// The descriptor itself.
    pub(crate) const fn slab(self) -> &'a Slab {
        self.0
    }

    /// Prepares an unused descriptor for a fresh slab of `class` whose first block is at `start`.
    ///
    /// # Safety
    ///
    /// `start` must point to a slab-sized, suitably aligned region that nothing else uses.
    pub(crate) unsafe fn init(self, class: Class, start: *mut u8) {
        let slab = self.0;
        slab.free.set(ptr::null_mut());
        slab.next.set(ptr::null());
        slab.prev.set(ptr::null());
        slab.start.set(start);
        slab.free_len.set(0);
        slab.bumped.set(0);
        slab.capacity.set(class.blocks_per_slab() as u32);
        slab.class.set(class.index() as u8);
        slab.list.set(List::None);
    }

    /// The size class of this slab's blocks.
    pub(crate) fn class(self) -> Class {
        Class::from_index(usize::from(self.0.class.get()))
    }

    /// The first block of the slab, which is also the slab's start address.
    pub(crate) const fn start(self) -> *mut u8 {
        self.0.start.get()
    }

    /// How many blocks are handed out, held in bins or on their way back in messages.
    pub(crate) const fn used(self) -> u32 {
        self.0.bumped.get() - self.0.free_len.get()
    }

    /// Whether every block is in use, so a refill can take nothing from this slab.
    pub(crate) const fn is_exhausted(self) -> bool {
        self.0.free_len.get() == 0 && self.0.bumped.get() == self.0.capacity.get()
    }

    /// The list this slab is on.
    pub(crate) const fn list(self) -> List {
        self.0.list.get()
    }

    /// Records which list this slab is on.
    pub(crate) fn set_list(self, list: List) {
        self.0.list.set(list);
    }

    /// The next slab on this slab's list.
    pub(crate) const fn next(self) -> *const Slab {
        self.0.next.get()
    }

    /// Sets the next slab on this slab's list.
    pub(crate) fn set_next(self, next: *const Slab) {
        self.0.next.set(next);
    }

    /// The previous slab on this slab's list.
    pub(crate) const fn prev(self) -> *const Slab {
        self.0.prev.get()
    }

    /// Sets the previous slab on this slab's list.
    pub(crate) fn set_prev(self, prev: *const Slab) {
        self.0.prev.set(prev);
    }

    /// Takes a block back.
    #[cfg(all(test, not(loom)))]
    ///
    /// # Safety
    ///
    /// `block` must be a block of this slab that is in use and that nobody will use again.
    pub(crate) unsafe fn push(self, block: *mut Block) {
        let slab = self.0;
        // SAFETY: the caller has given us the block.
        unsafe { (*block).next = slab.free.get() };
        slab.free.set(block);
        slab.free_len.set(slab.free_len.get() + 1);
    }

    /// Takes back `count` blocks that the caller has linked from `head` to `tail`.
    ///
    /// # Safety
    ///
    /// The blocks must be blocks of this slab that are in use and that nobody will use again,
    /// linked through their first words. The link of `tail` is overwritten.
    pub(crate) unsafe fn push_chain(self, head: *mut Block, tail: *mut Block, count: u32) {
        let slab = self.0;
        // SAFETY: the caller has given us the blocks.
        unsafe { (*tail).next = slab.free.get() };
        slab.free.set(head);
        slab.free_len.set(slab.free_len.get() + count);
    }

    /// Hands up to `max` free blocks to `out` and returns how many it handed out. Blocks that
    /// came back come first, then never-used blocks from the frontier, which are handed out from
    /// the highest address down so that a stack that pops the last one first walks memory upwards.
    pub(crate) fn take(self, max: usize, mut out: impl FnMut(*mut Block)) -> usize {
        let slab = self.0;
        let mut taken = 0;
        let mut block = slab.free.get();
        while taken < max && !block.is_null() {
            out(block);
            // SAFETY: blocks on the free list are unused memory whose first word is a link.
            block = unsafe { (*block).next };
            taken += 1;
        }
        slab.free.set(block);
        slab.free_len.set(slab.free_len.get() - taken as u32);
        let (bumped, capacity) = (slab.bumped.get() as usize, slab.capacity.get() as usize);
        let fresh = (max - taken).min(capacity - bumped);
        let size = self.class().block_size();
        for index in (bumped..bumped + fresh).rev() {
            // SAFETY: the block lies within the slab, because `capacity` blocks of `size` bytes
            // fit.
            out(unsafe { slab.start.get().add(index * size) }.cast());
        }
        slab.bumped.set((bumped + fresh) as u32);
        taken + fresh
    }

    /// A snapshot of the counters and the free list, for the consistency checks.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const fn inspect(self) -> Inspection {
        let slab = self.0;
        Inspection {
            free: slab.free.get(),
            free_len: slab.free_len.get(),
            bumped: slab.bumped.get(),
            capacity: slab.capacity.get(),
            start: slab.start.get(),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use std::vec::Vec;

    use core::mem::{align_of, size_of};

    use super::*;
    use crate::testing::Buffer;

    #[test]
    fn a_descriptor_fills_whole_cache_lines() {
        let line = align_of::<CacheLine>();
        assert_eq!(align_of::<Slab>(), line);
        assert_eq!(size_of::<Slab>() % line, 0);
    }

    /// A slab of `class` over its own memory, with the descriptor initialised.
    struct Fixture {
        slab: Slab,
        _memory: Buffer,
    }

    impl Fixture {
        fn new(size: usize) -> Self {
            let class = Class::for_size(size);
            let memory = Buffer::new(class.slab_chunks() << crate::chunks::CHUNK_SHIFT);
            let slab = Slab::new();
            // SAFETY: the buffer is slab sized and aligned, and ours alone.
            unsafe { slab.owned().init(class, memory.as_ptr()) };
            Self { slab, _memory: memory }
        }

        const fn owned(&self) -> Owned<'_> {
            // SAFETY: the fixture is used from one thread.
            unsafe { self.slab.owned() }
        }

        fn take(&self, max: usize) -> Vec<*mut Block> {
            let mut blocks = Vec::new();
            assert_eq!(self.owned().take(max, |block| blocks.push(block)), blocks.len());
            blocks
        }
    }

    #[test]
    fn a_fresh_slab_yields_every_block_once_and_pops_upwards() {
        let fixture = Fixture::new(1024);
        let blocks = fixture.take(1000);
        assert_eq!(blocks.len(), 64);
        let mut sorted = blocks.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 64);
        // The last block handed out is the lowest, so a stack pops them in address order.
        assert!(blocks.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(fixture.owned().is_exhausted());
        assert_eq!(fixture.owned().used(), 64);
        assert!(fixture.take(1).is_empty());
    }

    #[test]
    fn returned_blocks_come_back_before_fresh_ones() {
        let fixture = Fixture::new(256);
        let first = fixture.take(4);
        // SAFETY: the blocks are in use and given back once.
        unsafe { fixture.owned().push(first[1]) };
        assert_eq!(fixture.owned().used(), 3);
        let again = fixture.take(2);
        assert_eq!(again[0], first[1], "a returned block is reused first");
        assert!(!first.contains(&again[1]), "then the frontier moves on");
        assert_eq!(fixture.owned().used(), 5);
    }

    #[test]
    fn an_all_zero_descriptor_is_an_unused_one() {
        // SAFETY: an all-zero bit pattern is valid for every field.
        let zeroed: Slab = unsafe { core::mem::zeroed() };
        let owned = Owned(&zeroed);
        assert_eq!(owned.list(), List::None);
        assert_eq!(owned.used(), 0);
        assert!(owned.next().is_null() && owned.prev().is_null());
    }
}
