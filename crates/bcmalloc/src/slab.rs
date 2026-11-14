//! A slab, which is a run of chunks carved into equal blocks of one size class.
//!
//! Every slab has a descriptor, kept outside the slab so that user memory is never metadata. Only
//! the heap that owns the slab reads or writes it. Other threads that free a block of the slab send
//! the block to the owning heap instead, as the [heap module](crate::heap) explains, so the
//! descriptor needs no atomic operations at all.
//!
//! ```text
//!   slab memory, one size class (64-byte blocks here)
//!   +-------+-------+-------+-------+-------+-------+-------+-------+------
//!   | block | block | block | block | block | block | block | block |  ...
//!   +-------+-------+-------+-------+-------+-------+-------+-------+------
//!
//!   descriptor, owner only
//!   +---------------------------------------------+
//!   | free bitmap  0 1 1 0 0 0 1 0 ...            |  one bit per block, set while it is free
//!   | free_len, capacity, cursor, class, list     |
//!   | start, next, prev                           |
//!   +---------------------------------------------+
//! ```
//!
//! # Free blocks
//!
//! The descriptor keeps a bitmap with a bit for every block, set while the block is free. Taking a
//! block back sets its bit, and a refill collects set bits a word at a time, lowest first, so a
//! heap hands out a slab's free blocks in address order. Neither ever touches the memory of a
//! block, which is what jemalloc does with its slab bitmaps. A list threaded through the free
//! blocks would make a refill read every block in turn, each read waiting for the one before, and
//! make every free write into a block whose cache line may be cold.
//!
//! A slab counts every block whose bit is clear as in use. That includes blocks sitting in a heap's
//! bins or in a message on the way back, so a slab is only released when every block has really
//! come home.

use core::{cell::Cell, ptr};

use crate::{cache::CacheLine, size_class::Class};

/// A free block seen as a link, which is how a single block travels to its owner's inbox. Blocks
/// are at least 16 bytes.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct Block {
    pub(crate) next: *mut Self,
}

/// The most blocks a slab holds, which is a 64 KiB slab of 16-byte blocks.
const MAX_BLOCKS: usize = 4096;

/// The words of a slab's free bitmap.
const BITMAP_WORDS: usize = MAX_BLOCKS / 64;

/// Which of the owner's slab lists a slab is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum List {
    /// On neither list. This is an unused descriptor or a slab in the middle of a move.
    None = 0,
    /// Has free blocks, and refills draw from here.
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
    next: Cell<*const Self>,
    prev: Cell<*const Self>,
    start: Cell<*mut u8>,
    free_len: Cell<u32>,
    capacity: Cell<u32>,
    /// No word of the bitmap below this one has a bit set.
    cursor: Cell<u16>,
    /// Blocks from this index on have never been handed out since the slab was set up on memory
    /// that read as zeros, so they still hold zeros. It is the capacity if the memory was not
    /// zero.
    zero_from: Cell<u16>,
    class: Cell<u8>,
    list: Cell<List>,
    /// Whether every block has been handed out once since the slab was set up.
    filled: Cell<bool>,
    bits: [Cell<u64>; BITMAP_WORDS],
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
            next: Cell::new(ptr::null()),
            prev: Cell::new(ptr::null()),
            start: Cell::new(ptr::null_mut()),
            free_len: Cell::new(0),
            capacity: Cell::new(0),
            cursor: Cell::new(0),
            zero_from: Cell::new(0),
            class: Cell::new(0),
            list: Cell::new(List::None),
            filled: Cell::new(false),
            bits: [const { Cell::new(0) }; BITMAP_WORDS],
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

/// A descriptor in the hands of its owner, obtained from [`Slab::owned`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owned<'a>(&'a Slab);

impl<'a> Owned<'a> {
    /// The descriptor itself.
    pub(crate) const fn slab(self) -> &'a Slab {
        self.0
    }

    /// Prepares an unused descriptor for a fresh slab of `class` whose first block is at `start`,
    /// with every block free. `zeroed` says that the slab's memory reads as zeros.
    ///
    /// # Safety
    ///
    /// `start` must point to a slab-sized, suitably aligned region that nothing else uses, and
    /// that is all zeros if `zeroed`.
    pub(crate) unsafe fn init(self, class: Class, start: *mut u8, zeroed: bool) {
        let slab = self.0;
        let capacity = class.blocks_per_slab();
        debug_assert!(capacity <= MAX_BLOCKS);
        slab.next.set(ptr::null());
        slab.prev.set(ptr::null());
        slab.start.set(start);
        slab.free_len.set(capacity as u32);
        slab.capacity.set(capacity as u32);
        slab.cursor.set(0);
        slab.zero_from.set(if zeroed { 0 } else { capacity as u16 });
        slab.class.set(class.index() as u8);
        slab.list.set(List::None);
        slab.filled.set(false);
        for (index, word) in slab.bits.iter().enumerate() {
            let below = capacity.saturating_sub(index * 64);
            word.set(if below >= 64 { u64::MAX } else { (1 << below) - 1 });
        }
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
        self.0.capacity.get() - self.0.free_len.get()
    }

    /// Whether every block is in use, so a refill can take nothing from this slab.
    pub(crate) const fn is_exhausted(self) -> bool {
        self.0.free_len.get() == 0
    }

    /// Records that every block has been handed out, and returns whether this is the first time
    /// since the slab was set up.
    pub(crate) const fn mark_filled(self) -> bool {
        !self.0.filled.replace(true)
    }

    /// Whether every block has been handed out at some point since the slab was set up.
    pub(crate) const fn was_filled(self) -> bool {
        self.0.filled.get()
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
    ///
    /// # Safety
    ///
    /// `block` must be a block of this slab that is in use and that nobody will use again.
    pub(crate) unsafe fn push(self, block: *mut Block) {
        let mut returns = Returns::new(self);
        // SAFETY: forwarded.
        unsafe { returns.add(block) };
        returns.finish();
    }

    /// Hands up to `max` free blocks to `out`, lowest address first, and returns how many it handed
    /// out.
    pub(crate) fn take(self, max: usize, mut out: impl FnMut(*mut u8)) -> usize {
        let slab = self.0;
        let want = max.min(slab.free_len.get() as usize);
        let (start, size) = (slab.start.get(), self.class().block_size());
        let mut word = usize::from(slab.cursor.get());
        let mut taken = 0;
        let mut last = 0;
        while taken < want {
            let mut bits = slab.bits[word].get();
            if bits == u64::MAX && want - taken >= 64 {
                // A word of fresh or wholly returned blocks needs no bit search.
                // SAFETY: the 64 blocks of a full word lie in the slab.
                let mut block = unsafe { start.add(word * 64 * size) };
                for _ in 0..64 {
                    out(block);
                    block = block.wrapping_add(size);
                }
                slab.bits[word].set(0);
                taken += 64;
                last = word * 64 + 63;
                word += 1;
                continue;
            }
            while bits != 0 && taken < want {
                let index = word * 64 + bits.trailing_zeros() as usize;
                // SAFETY: a set bit stands for a block inside the slab.
                out(unsafe { start.add(index * size) });
                bits &= bits - 1;
                taken += 1;
                last = index;
            }
            slab.bits[word].set(bits);
            if bits == 0 {
                word += 1;
            }
        }
        slab.cursor.set(word.min(BITMAP_WORDS - 1) as u16);
        slab.free_len.set(slab.free_len.get() - taken as u32);
        if taken != 0 {
            // Blocks come out lowest first, so everything handed out lies below the last one.
            slab.zero_from.set(slab.zero_from.get().max(last as u16 + 1));
        }
        taken
    }

    /// Hands out the lowest free block, and whether it still holds the zeros of fresh memory.
    pub(crate) fn take_one(self) -> Option<(*mut u8, bool)> {
        let zero_from = usize::from(self.0.zero_from.get());
        let mut block = None;
        self.take(1, |taken| block = Some(taken));
        let block = block?;
        let index = (block.addr() - self.start().addr()) / self.class().block_size();
        Some((block, index >= zero_from))
    }

    /// Checks that the bitmap agrees with the counters and has no bit set below the cursor or
    /// past the last block, and returns how many blocks are free.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn check(self) -> usize {
        let slab = self.0;
        let capacity = slab.capacity.get() as usize;
        let mut count = 0;
        for (word, bits) in slab.bits.iter().enumerate() {
            let bits = bits.get();
            assert!(
                bits == 0 || word >= usize::from(slab.cursor.get()),
                "a free block sits below the cursor"
            );
            let valid = capacity.saturating_sub(word * 64);
            if valid < 64 {
                assert_eq!(bits >> valid, 0, "a bit past the last block is set");
            }
            count += bits.count_ones() as usize;
        }
        assert_eq!(count, slab.free_len.get() as usize, "free blocks disagree with the counter");
        count
    }

    /// Whether `block`, a block of this slab, is free.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn is_free(self, block: *mut u8) -> bool {
        let index = self.class().index_of(block.addr() - self.0.start.get().addr());
        self.0.bits[index / 64].get() & 1 << (index % 64) != 0
    }
}

/// Blocks being given back to one slab, with the bitmap word and the counter kept in registers
/// until [`finish`](Self::finish), so that a run of returns does not wait on its own stores.
pub(crate) struct Returns<'a> {
    slab: Owned<'a>,
    start: usize,
    class: Class,
    word: usize,
    mask: u64,
    count: u32,
    lowest: usize,
}

impl<'a> Returns<'a> {
    /// Starts giving blocks back to `slab`.
    #[inline(always)]
    pub(crate) fn new(slab: Owned<'a>) -> Self {
        let lowest = usize::from(slab.0.cursor.get());
        Self {
            slab,
            start: slab.start().addr(),
            class: slab.class(),
            word: 0,
            mask: 0,
            count: 0,
            lowest,
        }
    }

    /// The slab the blocks go to.
    pub(crate) const fn slab(&self) -> Owned<'a> {
        self.slab
    }

    /// Gives a block back.
    ///
    /// # Safety
    ///
    /// `block` must be a block of the slab that is in use and that nobody will use again, and no
    /// block may be given back twice.
    #[inline(always)]
    pub(crate) unsafe fn add(&mut self, block: *mut Block) {
        let index = self.class.index_of(block.addr() - self.start);
        let word = index / 64;
        if word != self.word {
            self.store();
            self.word = word;
        }
        debug_assert_eq!(self.mask & 1 << (index % 64), 0, "a block came back twice");
        self.mask |= 1 << (index % 64);
        self.count += 1;
    }

    /// Writes the pending bits of the current word.
    #[inline(always)]
    fn store(&mut self) {
        if self.mask != 0 {
            let bits = &self.slab.0.bits[self.word];
            debug_assert_eq!(bits.get() & self.mask, 0, "a block came back twice");
            bits.set(bits.get() | self.mask);
            self.lowest = self.lowest.min(self.word);
            self.mask = 0;
        }
    }

    /// Writes everything back to the descriptor and returns the slab.
    #[inline(always)]
    pub(crate) fn finish(mut self) -> Owned<'a> {
        self.store();
        let slab = self.slab.0;
        slab.free_len.set(slab.free_len.get() + self.count);
        slab.cursor.set(self.lowest as u16);
        self.slab
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

    #[test]
    fn the_bitmap_covers_the_fullest_slab() {
        for index in 0..crate::size_class::CLASSES {
            assert!(Class::from_index(index).blocks_per_slab() <= MAX_BLOCKS);
        }
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
            unsafe { slab.owned().init(class, memory.as_ptr(), true) };
            Self { slab, _memory: memory }
        }

        const fn owned(&self) -> Owned<'_> {
            // SAFETY: the fixture is used from one thread.
            unsafe { self.slab.owned() }
        }

        fn take(&self, max: usize) -> Vec<*mut u8> {
            let mut blocks = Vec::new();
            assert_eq!(self.owned().take(max, |block| blocks.push(block)), blocks.len());
            blocks
        }
    }

    #[test]
    fn a_fresh_slab_yields_every_block_once_in_address_order() {
        for size in [16, 48, 1024, 3000, 60_000] {
            let fixture = Fixture::new(size);
            let capacity = Class::for_size(size).blocks_per_slab();
            let blocks = fixture.take(usize::MAX);
            assert_eq!(blocks.len(), capacity);
            assert!(blocks.windows(2).all(|pair| pair[0] < pair[1]), "{size}");
            assert!(fixture.owned().is_exhausted());
            assert_eq!(fixture.owned().used(), capacity as u32);
            assert!(fixture.take(1).is_empty());
            assert_eq!(fixture.owned().check(), 0);
        }
    }

    #[test]
    fn returned_blocks_come_back_lowest_first() {
        let fixture = Fixture::new(256);
        let first = fixture.take(200);
        // SAFETY: the blocks are in use and given back once.
        unsafe {
            fixture.owned().push(first[150].cast());
            fixture.owned().push(first[3].cast());
        }
        assert_eq!(fixture.owned().used(), 198);
        let again = fixture.take(3);
        assert_eq!(again[..2], [first[3], first[150]]);
        assert!(!first.contains(&again[2]), "then fresh blocks follow");
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
