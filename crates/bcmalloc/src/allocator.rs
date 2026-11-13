//! The [`Allocator`](core::alloc::Allocator) trait, for collections that take an allocator as a
//! parameter.
//!
//! [`Bcmalloc`](crate::Bcmalloc) and [`Heap`](crate::Heap) both implement it, so a `Vec` or a
//! `Box` can allocate from one of them without it being the program's global allocator. Their
//! documentation has examples.
//!
//! # How the trait's rules are met
//!
//! * A zero-sized request never reaches the allocator. It gets a dangling pointer that is aligned
//!   as asked, and freeing it does nothing.
//! * A block is as long as it was asked to be. The allocator may have rounded the size up
//!   internally, but the slice it returns is exactly the requested length, so the zeroing and
//!   copying rules stay simple.
//! * Zeroed requests read as zeros in the whole block. Fresh memory is not cleared again, and
//!   everything else is.
//! * Growing and shrinking keep the block in place when the new size falls in the same size class.
//!   Otherwise they move it, and they always move it when the alignment changes, which the
//!   allocator's own resize does not support.

use core::{
    alloc::{AllocError, Layout},
    num::NonZero,
    ptr::{self, NonNull},
};

/// The three operations that the allocator trait is built from.
pub(crate) trait Backend {
    /// Allocates a block, or returns null.
    ///
    /// # Safety
    ///
    /// The layout must not be zero sized.
    unsafe fn allocate_raw(&self, layout: Layout) -> *mut u8;

    /// Allocates a zeroed block, or returns null.
    ///
    /// # Safety
    ///
    /// The layout must not be zero sized.
    unsafe fn allocate_zeroed_raw(&self, layout: Layout) -> *mut u8;

    /// Frees a block.
    ///
    /// # Safety
    ///
    /// The block must be live, from this allocator, and allocated with exactly this layout.
    unsafe fn deallocate_raw(&self, ptr: NonNull<u8>, layout: Layout);

    /// Resizes a block that keeps its alignment, returning null and leaving it untouched on
    /// failure.
    ///
    /// # Safety
    ///
    /// The block must be live with exactly this layout, and the new size must be valid for the
    /// alignment.
    unsafe fn resize_raw(&self, ptr: NonNull<u8>, old: Layout, new_size: usize) -> *mut u8;
}

/// A dangling block of no bytes that is aligned as the layout asks.
const fn empty(layout: Layout) -> NonNull<[u8]> {
    let address = NonZero::new(layout.align()).expect("alignments are not zero");
    NonNull::slice_from_raw_parts(NonNull::<u8>::without_provenance(address), 0)
}

fn block(ptr: *mut u8, size: usize) -> Result<NonNull<[u8]>, AllocError> {
    NonNull::new(ptr).map(|ptr| NonNull::slice_from_raw_parts(ptr, size)).ok_or(AllocError)
}

pub(crate) fn allocate<A: Backend + ?Sized>(
    backend: &A,
    layout: Layout,
    zeroed: bool,
) -> Result<NonNull<[u8]>, AllocError> {
    if layout.size() == 0 {
        return Ok(empty(layout));
    }
    // SAFETY: the layout is not zero sized.
    let ptr = unsafe {
        if zeroed { backend.allocate_zeroed_raw(layout) } else { backend.allocate_raw(layout) }
    };
    block(ptr, layout.size())
}

pub(crate) unsafe fn deallocate<A: Backend + ?Sized>(
    backend: &A,
    ptr: NonNull<u8>,
    layout: Layout,
) {
    if layout.size() != 0 {
        // SAFETY: the caller vouches for the block.
        unsafe { backend.deallocate_raw(ptr, layout) };
    }
}

/// Grows or shrinks a block. `zeroed` asks for the bytes beyond the old size to be zero.
///
/// # Safety
///
/// The block must be live with the old layout. The new size must be at least the old size when
/// growing and at most the old size when shrinking.
pub(crate) unsafe fn resize<A: Backend + ?Sized>(
    backend: &A,
    ptr: NonNull<u8>,
    old: Layout,
    new: Layout,
    zeroed: bool,
) -> Result<NonNull<[u8]>, AllocError> {
    if old.size() == 0 {
        return allocate(backend, new, zeroed);
    }
    if new.size() == 0 {
        // SAFETY: the caller vouches for the block, which is not zero sized.
        unsafe { backend.deallocate_raw(ptr, old) };
        return Ok(empty(new));
    }
    let moved = if old.align() == new.align() {
        // SAFETY: the block is live with the old layout, and the new layout is valid.
        unsafe { backend.resize_raw(ptr, old, new.size()) }
    } else {
        // SAFETY: the new layout is not zero sized.
        let moved = unsafe { backend.allocate_raw(new) };
        if !moved.is_null() {
            // SAFETY: both blocks are live and distinct, and the smaller size fits in each.
            unsafe {
                ptr::copy_nonoverlapping(ptr.as_ptr(), moved, old.size().min(new.size()));
                backend.deallocate_raw(ptr, old);
            }
        }
        moved
    };
    if zeroed && !moved.is_null() && new.size() > old.size() {
        // SAFETY: the block holds the new size, and the bytes past the old size are unused.
        unsafe { moved.add(old.size()).write_bytes(0, new.size() - old.size()) };
    }
    block(moved, new.size())
}

/// Implements [`Allocator`](core::alloc::Allocator) for a type that implements [`Backend`].
macro_rules! allocator {
    ($($header:tt)*) => {
        // SAFETY: the blocks come from the backend, which hands out valid, aligned, disjoint memory
        // that stays valid until it is freed, however the handle is moved or copied. Zero-sized
        // blocks are dangling and aligned, as the trait allows.
        unsafe impl $($header)* {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                $crate::allocator::allocate(self, layout, false)
            }

            fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                $crate::allocator::allocate(self, layout, true)
            }

            unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
                // SAFETY: forwarded.
                unsafe { $crate::allocator::deallocate(self, ptr, layout) }
            }

            unsafe fn grow(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                // SAFETY: forwarded.
                unsafe { $crate::allocator::resize(self, ptr, old, new, false) }
            }

            unsafe fn grow_zeroed(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                // SAFETY: forwarded.
                unsafe { $crate::allocator::resize(self, ptr, old, new, true) }
            }

            unsafe fn shrink(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                // SAFETY: forwarded.
                unsafe { $crate::allocator::resize(self, ptr, old, new, false) }
            }
        }
    };
}
pub(crate) use allocator;

#[cfg(all(test, not(loom)))]
mod test {
    use core::alloc::Allocator;
    use std::{boxed::Box, vec::Vec};

    use super::*;
    use crate::testing::{TestRegion, iterations};

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// The bytes of a block, for comparing contents.
    const fn bytes(block: NonNull<[u8]>) -> &'static [u8] {
        // SAFETY: the block is live for the duration of each test that calls this.
        unsafe { core::slice::from_raw_parts(block.as_ptr().cast::<u8>(), block.len()) }
    }

    #[test]
    fn collections_allocate_from_a_heap() {
        // The biggest slabs span 64 chunks, so the region has to have room for more than one.
        let test = TestRegion::new(160, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let mut numbers = Vec::new_in(&heap);
        numbers.extend(0..iterations(50_000) as u64);
        assert!(numbers.iter().enumerate().all(|(i, &n)| n == i as u64));
        let boxed = Box::new_in([7_u8; 300], &heap);
        assert_eq!(boxed[299], 7);
        drop((numbers, boxed));
        heap.validate();
    }

    #[test]
    fn zero_sized_blocks_are_aligned_dangling_and_free_to_drop() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let before = region.stats();
        let block = heap.allocate(layout(0, 64)).unwrap();
        assert_eq!((block.len(), block.as_ptr().cast::<u8>().addr() % 64), (0, 0));
        // SAFETY: the block came from this allocator with this layout.
        unsafe { heap.deallocate(block.cast(), layout(0, 64)) };
        assert_eq!(region.stats(), before, "an empty block takes no memory");
    }

    #[test]
    fn zeroed_requests_clear_memory_that_was_used_before() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let layout = layout(200, 8);
        let dirty: Vec<_> = (0..300)
            .map(|_| {
                let block = heap.allocate(layout).unwrap();
                // SAFETY: the block holds 200 bytes.
                unsafe { block.as_ptr().cast::<u8>().write_bytes(0xAA, 200) };
                block
            })
            .collect();
        for block in &dirty {
            // SAFETY: the block came from this allocator with this layout and is freed once.
            unsafe { heap.deallocate(block.cast(), layout) };
        }
        // A slab holds 327 of these blocks, so asking for that many covers every block that was
        // dirtied.
        let clean: Vec<_> = (0..327).map(|_| heap.allocate_zeroed(layout).unwrap()).collect();
        assert!(clean.iter().all(|block| bytes(*block).iter().all(|&byte| byte == 0)));
        assert!(
            clean
                .iter()
                .any(|block| dirty.iter().any(|old| old.cast::<u8>() == block.cast::<u8>())),
            "some of the memory was reused"
        );
    }

    #[test]
    fn growing_keeps_the_contents_and_zeroes_the_new_tail() {
        let test = TestRegion::new(64, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let (old, new) = (layout(40, 8), layout(5000, 8));
        let block = heap.allocate(old).unwrap();
        // SAFETY: the block holds 40 bytes.
        unsafe { block.as_ptr().cast::<u8>().write_bytes(1, 40) };
        // SAFETY: the block is live with the old layout and the new size is larger.
        let grown = unsafe { heap.grow_zeroed(block.cast(), old, new) }.unwrap();
        assert_eq!(grown.len(), 5000);
        assert!(bytes(grown)[..40].iter().all(|&byte| byte == 1));
        assert!(bytes(grown)[40..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn a_resize_within_a_size_class_keeps_the_block_in_place() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let block = heap.allocate(layout(20, 8)).unwrap();
        // SAFETY: the block is live with the old layout and the new size is larger.
        let grown = unsafe { heap.grow(block.cast(), layout(20, 8), layout(30, 8)) }.unwrap();
        assert_eq!(grown.cast::<u8>(), block.cast::<u8>());
        // SAFETY: the block is live with the layout it was grown to, and the new size is smaller.
        let shrunk = unsafe { heap.shrink(grown.cast(), layout(30, 8), layout(18, 8)) }.unwrap();
        assert_eq!(shrunk.cast::<u8>(), block.cast::<u8>());
    }

    #[test]
    fn a_resize_that_changes_the_alignment_moves_the_block_and_keeps_the_contents() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let old = layout(100, 8);
        let block = heap.allocate(old).unwrap();
        // SAFETY: the block holds 100 bytes.
        unsafe { block.as_ptr().cast::<u8>().write_bytes(5, 100) };
        // SAFETY: the block is live with the old layout and the new size is larger.
        let grown = unsafe { heap.grow(block.cast(), old, layout(300, 256)) }.unwrap();
        assert_eq!(grown.cast::<u8>().as_ptr().addr() % 256, 0);
        assert!(bytes(grown)[..100].iter().all(|&byte| byte == 5));
        // SAFETY: the block is live with the layout it was grown to, and the new size is smaller.
        let shrunk =
            unsafe { heap.shrink(grown.cast(), layout(300, 256), layout(50, 16)) }.unwrap();
        assert_eq!(shrunk.cast::<u8>().as_ptr().addr() % 16, 0);
        assert!(bytes(shrunk).iter().all(|&byte| byte == 5));
    }

    #[test]
    fn shrinking_to_nothing_frees_the_block() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let block = heap.allocate(layout(5000, 8)).unwrap();
        // SAFETY: the block is live with the old layout, and zero is smaller.
        let empty = unsafe { heap.shrink(block.cast(), layout(5000, 8), layout(0, 8)) }.unwrap();
        assert_eq!(empty.len(), 0);
        heap.flush();
        assert_eq!(region.stats().free, region.stats().capacity, "the block was freed");
    }

    #[test]
    fn a_refused_grow_leaves_the_block_untouched() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        let old = layout(64, 8);
        let block = heap.allocate(old).unwrap();
        // SAFETY: the block holds 64 bytes.
        unsafe { block.as_ptr().cast::<u8>().write_bytes(9, 64) };
        // The whole region is eight chunks, so this cannot be satisfied.
        // SAFETY: the block is live with the old layout and the new size is larger.
        let refused = unsafe { heap.grow(block.cast(), old, layout(16 << 20, 8)) };
        assert!(refused.is_err());
        assert!(bytes(block).iter().all(|&byte| byte == 9));
    }

    #[test]
    fn an_unsupported_alignment_is_an_error_and_not_a_panic() {
        let test = TestRegion::new(8, 1);
        let region = test.region();
        let heap = region.heap().unwrap();
        assert!(heap.allocate(layout(8, 1 << 20)).is_err());
    }
}
