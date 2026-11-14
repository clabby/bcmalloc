//! Size classes and slab geometry.
//!
//! A request of up to 64 KiB is rounded up to one of 87 size classes. Classes are 8 bytes apart
//! from 16 up to 128 bytes, as in TCMalloc and mimalloc, so the many Rust values of three, five or
//! seven words fill their blocks. Above that there are eight classes per doubling, so rounding
//! wastes at most one part in eight.
//!
//! ```text
//!    16 24 32 ... 112 120 128 | 144 160 176 192 208 224 240 256 | 288 320 352 ... 64K
//!    <---- 15 classes, +8 ---->  <------- 8 classes, +16 ------->  <--- +32 --->  ...
//!                                 <-------- one doubling -------->
//! ```
//!
//! Each class is carved out of slabs. A slab is a run of chunks aligned to its own size, so the
//! first chunk of a slab is found by masking a chunk index. Bigger blocks get bigger slabs. That
//! keeps at least eight blocks in every slab and the unusable tail of a slab under an eighth of it.
//!
//! | Block size      | Slab        | Blocks per slab |
//! | --------------- | ----------- | --------------- |
//! | 16 B to 8 KiB   | 64 KiB (1)  | 8 to 4096       |
//! | 8 KiB to 64 KiB | 512 KiB (8) | 8 to 56         |
//!
//! Anything larger than 64 KiB is a huge request, which gets a run of whole chunks of its own,
//! as jemalloc gives large requests extents of their own. A slab of bigger blocks would hold so
//! few of them that one partly used slab per class and heap would cost more than rounding up to
//! whole chunks does.
//!
//! # Alignment
//!
//! A slab starts on a multiple of its own size and a block on a multiple of its size within the
//! slab, so a block is aligned to the largest power of two that divides its size. Every class is a
//! multiple of 8, so every block is 8-byte aligned. A request that asks for 16, which C's malloc
//! always does, is rounded up to a multiple of 16 first. A request for more alignment is rounded
//! up to a power of two that is at least as large as the alignment. Huge allocations start on a
//! chunk boundary, so the largest supported alignment is one chunk.

use core::alloc::Layout;

use crate::chunks::CHUNK_SIZE;

/// The alignment that C's malloc promises, which a request gets unless its layout asks for less.
pub(crate) const MIN_ALIGN: usize = 16;

/// How far apart the classes up to [`LINEAR_LIMIT`] are, which is also the alignment every block
/// has.
pub(crate) const STEP: usize = 8;

/// The largest alignment the allocator supports, which is one chunk.
pub const MAX_ALIGN: usize = CHUNK_SIZE;

/// The largest block served from a slab. Bigger requests are huge allocations.
pub const MAX_SMALL: usize = 64 << 10;

/// The largest block that a heap may reuse when another heap owns it. See
/// [`Class::reusable_anywhere`].
const ANYWHERE_MAX: usize = 256;

/// Requests of up to this many bytes are classified by a table lookup.
pub(crate) const TABLE_MAX: usize = 8192;

/// The number of size classes: fifteen up to 128 bytes, and eight per doubling up to
/// [`MAX_SMALL`].
pub(crate) const CLASSES: usize =
    LINEAR_CLASSES + 8 * (MAX_SMALL.ilog2() - LINEAR_LIMIT.ilog2()) as usize;

/// Classes up to this many bytes advance in steps of [`STEP`].
const LINEAR_LIMIT: usize = 128;

/// The smallest class. A slab's bitmap has room for the blocks of a single chunk of it.
const SMALLEST: usize = 16;

/// How many classes there are up to [`LINEAR_LIMIT`].
const LINEAR_CLASSES: usize = (LINEAR_LIMIT - SMALLEST) / STEP + 1;

/// A size class, which is an index into the table of block sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Class(u8);

/// What a [`Layout`] asks of the allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// A block from a slab of this class.
    Small(Class),
    /// A run of this many whole chunks.
    Huge { chunks: usize },
}

/// The block size of class index `i`.
const fn block_size_of(i: usize) -> usize {
    if i < LINEAR_CLASSES {
        return SMALLEST + i * STEP;
    }
    let (doubling, step) = ((i - LINEAR_CLASSES) / 8, (i - LINEAR_CLASSES) % 8);
    let top_bit = doubling + 7;
    (1 << top_bit) + (step + 1) * (1 << (top_bit - 3))
}

const BLOCK_SIZES: [u32; CLASSES] = {
    let mut sizes = [0; CLASSES];
    let mut i = 0;
    while i < CLASSES {
        sizes[i] = block_size_of(i) as u32;
        i += 1;
    }
    sizes
};

/// The cache line width that decides whether two blocks of a class can share a line. Apple silicon
/// has 128-byte lines, and x86-64 keeps coherence in 64-byte lines.
const LINE: usize = if cfg!(target_arch = "aarch64") { 128 } else { 64 };

/// How many block pointers a heap's bin of class index `i` starts out holding at most. Small blocks
/// get many and big blocks few, so that a full bin of a small class holds about 32 KiB and one of a
/// class above 8 KiB about 64 KiB.
const fn bin_initial_of(i: usize) -> usize {
    let size = block_size_of(i);
    if size <= TABLE_MAX {
        clamp((32 << 10) / size, 8, 128)
    } else {
        clamp((64 << 10) / size, 1, 8)
    }
}

/// How many block pointers the bin of class index `i` may grow to hold. A bin grows when the
/// program alternates between emptying it and filling it, so that bursts of a few thousand blocks
/// stay in the bin, as TCMalloc grows its per-CPU caches.
const fn bin_max_of(i: usize) -> usize {
    let size = block_size_of(i);
    if size <= TABLE_MAX {
        clamp((128 << 10) / size, 8, 2048)
    } else {
        clamp((128 << 10) / size, 1, 16)
    }
}

/// `value` limited to `low..=high`.
const fn clamp(value: usize, low: usize, high: usize) -> usize {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// Where each class's bin starts in a heap's slot array, and the total number of slots.
const BIN_STARTS: ([u16; CLASSES], usize) = {
    let mut starts = [0; CLASSES];
    let mut total = 0;
    let mut i = 0;
    while i < CLASSES {
        starts[i] = total as u16;
        total += bin_max_of(i);
        i += 1;
    }
    (starts, total)
};

/// The initial and the largest capacity of each class's bin, as tables so that they cost a load
/// and not a division.
const BIN_CAPACITIES: ([u16; CLASSES], [u16; CLASSES]) = {
    let (mut initial, mut max) = ([0; CLASSES], [0; CLASSES]);
    let mut i = 0;
    while i < CLASSES {
        initial[i] = bin_initial_of(i) as u16;
        max[i] = bin_max_of(i) as u16;
        i += 1;
    }
    (initial, max)
};

/// The number of block pointers in all the bins of one heap.
pub(crate) const BIN_SLOTS: usize = BIN_STARTS.1;

/// For each class, a multiplier that divides an offset into a slab by the block size, as jemalloc
/// divides by bin sizes. With `m = ceil(2^32 / size)`, `(offset * m) >> 32` is exact for every
/// multiple of `size` below 2^32 / size * size, and slabs are at most 4 MiB.
const RECIPROCALS: [u64; CLASSES] = {
    let mut table = [0; CLASSES];
    let mut i = 0;
    while i < CLASSES {
        table[i] = (1_u64 << 32).div_ceil(block_size_of(i) as u64);
        i += 1;
    }
    table
};

/// How many entries the class table has: one for every [`STEP`] up to [`TABLE_MAX`].
pub(crate) const TABLE_STEPS: usize = TABLE_MAX / STEP + 1;

/// The class of every size up to [`TABLE_MAX`], indexed by the size in steps of [`STEP`] rounded
/// up.
const TABLE: [u8; TABLE_STEPS] = {
    let mut table = [0; TABLE_STEPS];
    let mut step = 0;
    while step < table.len() {
        table[step] = Class::for_size(step * STEP).0;
        step += 1;
    }
    table
};

impl Class {
    /// The class of a size of at most [`TABLE_MAX`], for a request that needs no more than 8 bytes
    /// of alignment, or for one that needs 16 once the size is rounded up to a multiple of 16.
    #[inline(always)]
    pub(crate) const fn from_table(size: usize) -> Self {
        Self(TABLE[size.div_ceil(STEP)])
    }

    /// Whether the layout is small and aligned loosely enough to be classified by the table,
    /// which is the common case and a single load.
    #[inline(always)]
    pub(crate) const fn is_table_request(layout: Layout) -> bool {
        layout.size() <= TABLE_MAX && layout.align() <= MIN_ALIGN
    }

    /// The class with table index `index`.
    pub(crate) const fn from_index(index: usize) -> Self {
        assert!(index < CLASSES);
        Self(index as u8)
    }

    /// The class with table index `index`, without the check.
    ///
    /// # Safety
    ///
    /// `index` must be below [`CLASSES`].
    pub(crate) const unsafe fn from_index_unchecked(index: usize) -> Self {
        debug_assert!(index < CLASSES);
        Self(index as u8)
    }

    /// The smallest class whose blocks hold `size` bytes. `size` must be at most [`MAX_SMALL`].
    pub(crate) const fn for_size(size: usize) -> Self {
        debug_assert!(size <= MAX_SMALL);
        if size <= LINEAR_LIMIT {
            return Self(size.saturating_sub(SMALLEST).div_ceil(STEP) as u8);
        }
        let below = size - 1;
        let top_bit = (usize::BITS - 1 - below.leading_zeros()) as usize;
        let step = (below >> (top_bit - 3)) & 7;
        Self((LINEAR_CLASSES + (top_bit - 7) * 8 + step) as u8)
    }

    /// How many block pointers the bin of this class holds when a heap starts.
    pub(crate) const fn bin_initial(self) -> usize {
        BIN_CAPACITIES.0[self.0 as usize] as usize
    }

    /// How many block pointers the bin of this class may grow to hold.
    pub(crate) const fn bin_max(self) -> usize {
        BIN_CAPACITIES.1[self.0 as usize] as usize
    }

    /// Where the bin of this class starts in a heap's slot array.
    pub(crate) const fn bin_start(self) -> usize {
        BIN_STARTS.0[self.0 as usize] as usize
    }

    /// Whether a heap may keep blocks of this class that belong to another heap's slab and hand
    /// them out again.
    ///
    /// That needs two things. A block must not share a cache line with any other block, or the
    /// thread that reuses it would write next to a block that its owner still uses, which is the
    /// false sharing that Hoard's ownership rule exists to prevent. Blocks start at multiples of
    /// their size, so a size that is a multiple of the line width is enough. And the block must
    /// span few lines. Its lines are likely still in the cache of the owner, which wrote them, and
    /// a thread on another core that reuses the block pulls each line it writes across, where the
    /// owner would find them at hand. For a few lines that costs less than sending the block home.
    pub(crate) const fn reusable_anywhere(self) -> bool {
        let size = self.block_size();
        size.is_multiple_of(LINE) && size <= ANYWHERE_MAX
    }

    /// The position of this class in per-heap tables.
    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }

    /// The size of every block of this class.
    pub(crate) const fn block_size(self) -> usize {
        BLOCK_SIZES[self.0 as usize] as usize
    }

    /// The index of the block that starts `offset` bytes into a slab of this class, without a
    /// division. `offset` must be a multiple of the block size inside the slab.
    pub(crate) const fn index_of(self, offset: usize) -> usize {
        ((offset as u64 * RECIPROCALS[self.0 as usize]) >> 32) as usize
    }

    /// How many chunks one slab of this class spans, which is 1 or 8. Slabs are aligned to this
    /// many chunks.
    pub(crate) const fn slab_chunks(self) -> usize {
        if self.block_size() <= TABLE_MAX { 1 } else { 8 }
    }

    /// How many blocks fit in one slab.
    pub(crate) const fn blocks_per_slab(self) -> usize {
        self.slab_chunks() * CHUNK_SIZE / self.block_size()
    }
}

/// Decides how a layout is served, or `None` if it cannot be (alignment above [`MAX_ALIGN`]).
pub(crate) const fn classify(layout: Layout) -> Option<Request> {
    let (size, align) = (layout.size(), layout.align());
    if align > MAX_ALIGN {
        return None;
    }
    if size > MAX_SMALL {
        return Some(Request::Huge { chunks: size.div_ceil(CHUNK_SIZE) });
    }
    // Over-aligned blocks come from power-of-two classes, as the module documentation explains.
    let size = if align <= STEP {
        size
    } else if align == MIN_ALIGN {
        size.next_multiple_of(MIN_ALIGN)
    } else {
        let rounded = size.next_power_of_two();
        if rounded > align { rounded } else { align }
    };
    Some(Request::Small(Class::for_size(size)))
}

#[cfg(test)]
mod test {
    use super::*;

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn the_table_is_strictly_increasing_and_spans_the_range() {
        let sizes: std::vec::Vec<_> =
            (0..CLASSES).map(|i| Class::from_index(i).block_size()).collect();
        assert_eq!((sizes[0], sizes[CLASSES - 1]), (SMALLEST, MAX_SMALL));
        assert!(sizes.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(sizes.iter().all(|size| size % STEP == 0));
        assert!(sizes[LINEAR_CLASSES..].iter().all(|size| size % MIN_ALIGN == 0));
    }

    #[test]
    fn for_size_picks_the_smallest_class_that_fits() {
        // Every small size is cheap to check, and Miri takes a sample.
        let step = if cfg!(miri) { 997 } else { 1 };
        for size in (1..=MAX_SMALL).step_by(step) {
            let class = Class::for_size(size);
            assert!(class.block_size() >= size, "{size} does not fit {class:?}");
            if class.index() > 0 {
                let smaller = Class::from_index(class.index() - 1);
                assert!(smaller.block_size() < size, "{size} would fit {smaller:?}");
            }
        }
    }

    #[test]
    fn the_table_agrees_with_the_arithmetic() {
        for size in 0..=TABLE_MAX {
            assert_eq!(Class::from_table(size), Class::for_size(size), "{size}");
        }
    }

    #[test]
    fn table_classes_have_one_chunk_slabs() {
        // The inlined free path finds a descriptor by chunk index alone, which needs this.
        for size in 0..=TABLE_MAX {
            assert_eq!(Class::from_table(size).slab_chunks(), 1, "{size}");
        }
    }

    #[test]
    fn rounding_wastes_at_most_an_eighth() {
        for i in 1..CLASSES {
            let (class, below) = (Class::from_index(i), Class::from_index(i - 1));
            // The worst request for a class is one byte more than the class below it.
            let worst = below.block_size() + 1;
            if worst > LINEAR_LIMIT {
                assert!(
                    class.block_size() * 8 <= worst * 9,
                    "{class:?} wastes too much on {worst}"
                );
            }
        }
    }

    #[test]
    fn index_of_divides_every_block_offset_exactly() {
        for i in 0..CLASSES {
            let class = Class::from_index(i);
            for index in 0..class.blocks_per_slab() {
                assert_eq!(class.index_of(index * class.block_size()), index, "{class:?}");
            }
        }
    }

    #[test]
    fn slabs_hold_at_least_eight_blocks_and_waste_little() {
        for i in 0..CLASSES {
            let class = Class::from_index(i);
            assert!(class.slab_chunks().is_power_of_two());
            assert!(class.blocks_per_slab() >= 8, "{class:?}");
            let tail =
                class.slab_chunks() * CHUNK_SIZE - class.blocks_per_slab() * class.block_size();
            assert!(tail < class.block_size(), "{class:?} wastes a whole block");
        }
    }

    #[test]
    fn blocks_honour_the_requested_alignment() {
        for align_shift in 0..=16 {
            let align = 1usize << align_shift;
            for size in [1, 7, 16, 100, 4096, 5000, 70_000, MAX_SMALL, MAX_SMALL + 1, 3 << 20] {
                match classify(layout(size, align)).unwrap() {
                    Request::Small(class) => {
                        assert!(class.block_size() >= size);
                        // A slab starts at a multiple of its own size and block i starts i blocks
                        // in, so a block size that is a multiple of the alignment is enough.
                        assert_eq!(class.block_size() % align, 0, "{size}/{align}");
                    }
                    Request::Huge { chunks } => assert!(chunks * CHUNK_SIZE >= size),
                }
            }
        }
    }
    #[test]
    fn requests_for_sixteen_bytes_of_alignment_get_classes_that_keep_it() {
        for size in 1..=TABLE_MAX {
            let Some(Request::Small(class)) = classify(layout(size, MIN_ALIGN)) else { panic!() };
            assert_eq!(class.block_size() % MIN_ALIGN, 0, "{size}");
            assert_eq!(Class::from_table(size.next_multiple_of(MIN_ALIGN)), class);
            let Some(Request::Small(loose)) = classify(layout(size, STEP)) else { panic!() };
            assert!(loose.block_size() <= class.block_size());
            assert_eq!(Class::from_table(size), loose);
        }
        assert_eq!(Class::for_size(24).block_size(), 24, "three words fill their block");
    }

    #[test]
    fn the_small_and_huge_boundary_is_exact() {
        assert_eq!(
            classify(layout(MAX_SMALL, 1)),
            Some(Request::Small(Class::from_index(CLASSES - 1)))
        );
        assert_eq!(classify(layout(MAX_SMALL + 1, 1)), Some(Request::Huge { chunks: 2 }));
        assert_eq!(
            classify(layout(CHUNK_SIZE, 1)),
            Some(Request::Small(Class::for_size(CHUNK_SIZE)))
        );
    }

    #[test]
    fn alignment_beyond_a_chunk_is_refused() {
        assert_eq!(classify(layout(8, MAX_ALIGN * 2)), None);
        assert!(classify(layout(8, MAX_ALIGN)).is_some());
    }
}
