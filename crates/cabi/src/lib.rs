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
//! size, so `free`, `realloc` and `malloc_usable_size` read what the block is from the chunk map.
//! Pointers that the allocator does not own are ignored by `free`, which can happen with memory a
//! program got before the library was loaded. The C++ operators `new` and `delete` are defined
//! too, in the [`cxx`] module, so that C++ programs reach the same fast paths.
//!
//! This is for running C benchmarks and programs on Linux. On macOS, programs allocate through
//! malloc zones, which this library does not register.

#![allow(unsafe_code)]

pub mod cxx;

use bcmalloc::Bcmalloc;
use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::{c_int, c_void},
    ptr::{self, NonNull},
};

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
    unsafe { *errno_location() = code };
}

/// The calling thread's errno.
fn errno_location() -> *mut c_int {
    // SAFETY: the errno location is valid for the calling thread.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::__errno_location()
    }
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    unsafe {
        libc::__error()
    }
}

/// The size of a memory page, which valloc and pvalloc align to.
fn page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).unwrap_or(4096).max(1)
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
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // Bcmalloc::malloc sets errno itself when it fails.
    ALLOCATOR.malloc(size).cast()
}

/// C `free`.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    let Some(block) = NonNull::new(ptr.cast::<u8>()) else { return };
    // SAFETY: the caller frees a live block, and pointers from elsewhere are ignored.
    unsafe { ALLOCATOR.free_unsized(block) };
}

/// C `calloc`.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    // Bcmalloc::calloc sets errno itself when it fails.
    ALLOCATOR.calloc(count, size).cast()
}

/// C `realloc`. A size of zero frees the block and returns null, as glibc does.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    let Some(block) = NonNull::new(ptr.cast::<u8>()) else {
        return allocate(size, MIN_ALIGN);
    };
    if size == 0 {
        // SAFETY: forwarded.
        unsafe { free(ptr) };
        return ptr::null_mut();
    }
    // SAFETY: the caller passes a live block, and the size is not zero.
    let moved = unsafe { ALLOCATOR.realloc_unsized(block, size) };
    if moved.is_null() {
        set_errno(libc::ENOMEM);
    }
    moved.cast()
}

/// The obsolete `cfree`, the same as `free`.
///
/// # Safety
///
/// As for [`free`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn cfree(ptr: *mut c_void) {
    // SAFETY: forwarded.
    unsafe { free(ptr) };
}

/// C23 `free_sized`. The size is not needed, because the chunk map holds the block's class.
///
/// # Safety
///
/// As for [`free`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn free_sized(ptr: *mut c_void, _size: usize) {
    // SAFETY: forwarded.
    unsafe { free(ptr) };
}

/// C23 `free_aligned_sized`.
///
/// # Safety
///
/// As for [`free`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn free_aligned_sized(ptr: *mut c_void, _align: usize, _size: usize) {
    // SAFETY: forwarded.
    unsafe { free(ptr) };
}

/// `reallocarray`, a `realloc` of `count` items of `size` bytes that fails when the product
/// overflows.
///
/// # Safety
///
/// As for [`realloc`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn reallocarray(ptr: *mut c_void, count: usize, size: usize) -> *mut c_void {
    let Some(bytes) = count.checked_mul(size) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    // SAFETY: forwarded.
    unsafe { realloc(ptr, bytes) }
}

/// C11 `aligned_alloc`.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[cfg_attr(not(test), unsafe(no_mangle))]
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
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
    // SAFETY: forwarded.
    unsafe { aligned_alloc(align, size) }
}

/// POSIX `posix_memalign`.
///
/// # Safety
///
/// `out` must be valid for a write of a pointer.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int {
    if !align.is_power_of_two() || !align.is_multiple_of(size_of::<*mut c_void>()) {
        return libc::EINVAL;
    }
    // posix_memalign reports failure through its result and leaves errno alone.
    // SAFETY: the errno location is valid for the calling thread.
    let saved = unsafe { *errno_location() };
    let block = allocate(size, align);
    if block.is_null() {
        set_errno(saved);
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
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
    allocate(size, page_size())
}

/// The obsolete `pvalloc`, page aligned and rounded up to whole pages, at least one.
///
/// # Safety
///
/// Callable from C with the usual contract.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn pvalloc(size: usize) -> *mut c_void {
    let page = page_size();
    let Some(rounded) = size.max(1).checked_next_multiple_of(page) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    allocate(rounded, page)
}

/// glibc's `malloc_usable_size`: how many bytes the block really holds.
///
/// # Safety
///
/// `ptr` must be null or a live block from this library's allocation functions.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> usize {
    NonNull::new(ptr.cast::<u8>()).and_then(|block| ALLOCATOR.usable_size(block)).unwrap_or(0)
}

/// Writes the allocator's metrics to standard error. Built with the `metrics` feature.
#[cfg(feature = "metrics")]
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn bcmalloc_print_metrics() {
    if let Some(metrics) = ALLOCATOR.metrics() {
        let report = std::format!("{metrics}");
        // SAFETY: the buffer is valid for its length, and a short write only loses the report.
        unsafe { libc::write(2, report.as_ptr().cast(), report.len()) };
    }
}

/// Prints the metrics at exit when `BCMALLOC_SHOW_METRICS` is set, as `MIMALLOC_SHOW_STATS` does.
#[cfg(all(feature = "metrics", target_os = "linux"))]
extern "C" fn print_metrics_at_exit() {
    // SAFETY: the name is a valid C string, and the environment is not changing at exit.
    if !unsafe { libc::getenv(c"BCMALLOC_SHOW_METRICS".as_ptr()) }.is_null() {
        bcmalloc_print_metrics();
    }
}

/// Runs [`print_metrics_at_exit`] among the library's destructors.
#[cfg(all(feature = "metrics", target_os = "linux"))]
#[used]
#[unsafe(link_section = ".fini_array")]
static PRINT_METRICS_AT_EXIT: extern "C" fn() = print_metrics_at_exit;

#[cfg(test)]
mod test {
    //! The C functions are built under their Rust names here, so the test binary keeps the system
    //! allocator's symbols and calls these directly.

    use super::*;

    fn errno() -> c_int {
        // SAFETY: the errno location is valid for the calling thread.
        unsafe { *errno_location() }
    }

    #[test]
    fn calloc_refuses_a_size_that_overflows_and_zeroes_what_it_returns() {
        set_errno(0);
        // SAFETY: the C contract.
        assert!(unsafe { calloc(usize::MAX, 2) }.is_null());
        assert_eq!(errno(), libc::ENOMEM);
        // SAFETY: as above.
        let block = unsafe { calloc(100, 3) }.cast::<u8>();
        // SAFETY: the block holds 300 bytes.
        assert!(unsafe { core::slice::from_raw_parts(block, 300) }.iter().all(|&byte| byte == 0));
        // SAFETY: the block is live and freed once.
        unsafe { free(block.cast()) };
    }

    #[test]
    fn realloc_follows_the_c_rules_for_null_zero_and_failure() {
        // SAFETY: the C contract, for each call.
        unsafe {
            let block = realloc(ptr::null_mut(), 40).cast::<u8>();
            assert!(!block.is_null(), "a null pointer allocates");
            block.write_bytes(9, 40);
            set_errno(0);
            assert!(realloc(block.cast(), usize::MAX).is_null());
            assert_eq!(errno(), libc::ENOMEM);
            assert!(core::slice::from_raw_parts(block, 40).iter().all(|&b| b == 9), "kept");
            let grown = realloc(block.cast(), 5000).cast::<u8>();
            assert!(core::slice::from_raw_parts(grown, 40).iter().all(|&b| b == 9));
            assert!(malloc_usable_size(grown.cast()) >= 5000);
            assert!(realloc(grown.cast(), 0).is_null(), "a size of zero frees");
        }
    }

    #[test]
    fn aligned_allocations_check_their_alignment_and_keep_errno_where_posix_says() {
        // SAFETY: the C contract, for each call.
        unsafe {
            set_errno(0);
            assert!(aligned_alloc(24, 100).is_null());
            assert_eq!(errno(), libc::EINVAL);
            let mut out = ptr::dangling_mut::<c_void>();
            set_errno(7);
            assert_eq!(posix_memalign(&raw mut out, 4, 100), libc::EINVAL);
            assert_eq!(posix_memalign(&raw mut out, 1 << 20, 100), libc::ENOMEM);
            assert_eq!(out, ptr::dangling_mut(), "a failed call leaves the output alone");
            assert_eq!(errno(), 7, "and errno too");
            assert_eq!(posix_memalign(&raw mut out, 4096, 100), 0);
            assert!(out.addr().is_multiple_of(4096));
            free(out);
            let page = page_size();
            let block = pvalloc(1);
            assert!(block.addr().is_multiple_of(page) && malloc_usable_size(block) >= page);
            free(block);
            assert!(pvalloc(usize::MAX).is_null());
            assert_eq!(errno(), libc::ENOMEM);
            let block = memalign(256, 10);
            assert!(block.addr().is_multiple_of(256));
            free(block);
        }
    }

    #[test]
    fn small_requests_are_sixteen_byte_aligned_and_null_is_harmless() {
        // SAFETY: the C contract, for each call.
        unsafe {
            for size in [0, 1, 8, 24, 40, 100, 8192, 100_000] {
                let block = malloc(size);
                assert!(!block.is_null() && block.addr().is_multiple_of(MIN_ALIGN), "{size}");
                assert!(malloc_usable_size(block) >= size);
                free(block);
            }
            free(ptr::null_mut());
            assert_eq!(malloc_usable_size(ptr::null_mut()), 0);
            let mut outside = 0_u64;
            free((&raw mut outside).cast());
        }
    }
}
