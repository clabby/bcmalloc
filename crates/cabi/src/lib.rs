//! The C allocation interface of bcmalloc, for loading into programs that call malloc and free.
//!
//! Build it with `just cabi`, which makes a shared library that replaces the C allocator when it
//! is preloaded:
//!
//! ```text
//! LD_PRELOAD=target/cabi/release/libbcmalloc_c.so ./program
//! ```
//!
//! Every function forwards to one [`Bcmalloc`] with its default configuration. C frees carry no
//! size, so `free` and `realloc` ask the allocator for the layout of the block from its chunk map.
//! Pointers that the allocator does not own are ignored by `free`, which can happen with memory a
//! program got before the library was loaded.
//!
//! This is for running C benchmarks and programs on Linux. On macOS, programs allocate through
//! malloc zones, which this library does not register.

#![allow(unsafe_code)]

use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::{c_int, c_void},
    ptr::{self, NonNull},
};

use bcmalloc::Bcmalloc;

/// The allocator behind every function, and behind Rust's own allocations in this library.
#[global_allocator]
static ALLOCATOR: Bcmalloc = Bcmalloc::new();

/// The alignment that malloc guarantees, which is enough for any scalar type.
const MIN_ALIGN: usize = 16;

/// A layout of at least one byte with at least the alignment malloc guarantees.
fn layout(size: usize, align: usize) -> Option<Layout> {
    Layout::from_size_align(size.max(1), align.max(MIN_ALIGN)).ok()
}

/// Sets errno for the calling thread.
fn set_errno(code: c_int) {
    // SAFETY: the errno location is valid for the calling thread.
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = code;
    }
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = code;
    }
}

/// Allocates `size` bytes aligned to `align`, setting errno to ENOMEM on failure.
fn allocate(size: usize, align: usize) -> *mut c_void {
    let Some(layout) = layout(size, align) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { ALLOCATOR.alloc(layout) };
    if block.is_null() {
        set_errno(libc::ENOMEM);
    }
    block.cast()
}

/// C `malloc`.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    allocate(size, MIN_ALIGN)
}

/// C `free`.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    let Some(block) = NonNull::new(ptr.cast::<u8>()) else { return };
    if let Some(layout) = ALLOCATOR.block_layout(block) {
        // SAFETY: the caller frees a live block, and the layout is the one the allocator holds.
        unsafe { ALLOCATOR.dealloc(block.as_ptr(), layout) };
    }
}

/// C `calloc`.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(bytes) = count.checked_mul(size) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    let block = allocate(bytes, MIN_ALIGN);
    if !block.is_null() {
        // SAFETY: the block holds at least `bytes` bytes.
        unsafe { block.cast::<u8>().write_bytes(0, bytes) };
    }
    block
}

/// C `realloc`. A size of zero frees the block and returns null, as glibc does.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    let Some(block) = NonNull::new(ptr.cast::<u8>()) else {
        return allocate(size, MIN_ALIGN);
    };
    if size == 0 {
        // SAFETY: forwarded.
        unsafe { free(ptr) };
        return ptr::null_mut();
    }
    let Some(old) = ALLOCATOR.block_layout(block) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    if layout(size, MIN_ALIGN).is_none() {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    }
    // SAFETY: the block is live with the layout the allocator holds, and the size is non-zero and
    // valid for the alignment.
    let moved = unsafe { ALLOCATOR.realloc(block.as_ptr(), old, size) };
    if moved.is_null() {
        set_errno(libc::ENOMEM);
    }
    moved.cast()
}

/// C11 `aligned_alloc`.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
    if !align.is_power_of_two() {
        set_errno(libc::EINVAL);
        return ptr::null_mut();
    }
    allocate(size, align)
}

/// The obsolete `memalign`, which some programs and benchmarks still call.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
    // SAFETY: forwarded.
    unsafe { aligned_alloc(align, size) }
}

/// POSIX `posix_memalign`.
///
/// # Safety
///
/// `out` must be valid for a write of a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int {
    if !align.is_power_of_two() || !align.is_multiple_of(size_of::<*mut c_void>()) {
        return libc::EINVAL;
    }
    let block = allocate(size, align);
    if block.is_null() {
        return libc::ENOMEM;
    }
    // SAFETY: the caller passes a writable pointer.
    unsafe { *out = block };
    0
}

/// The obsolete `valloc`, page aligned.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
    allocate(size, 4096)
}

/// The obsolete `pvalloc`, page aligned and rounded up to whole pages.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pvalloc(size: usize) -> *mut c_void {
    allocate(size.next_multiple_of(4096), 4096)
}

/// glibc's `malloc_usable_size`: how many bytes the block really holds.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> usize {
    NonNull::new(ptr.cast::<u8>())
        .and_then(|block| ALLOCATOR.block_layout(block))
        .map_or(0, |layout| layout.size())
}
