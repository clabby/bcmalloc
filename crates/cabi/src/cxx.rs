//! The C++ allocation operators, `new` and `delete` in all their forms, as the preload libraries of
//! mimalloc, snmalloc and jemalloc define them too.
//!
//! Without them, a C++ program's `new` runs libstdc++'s operator, which calls `malloc` through the
//! dynamic linker's table and checks the result, and `delete` goes the same way to `free`. Defined
//! here, both reach the allocator's fast paths directly.
//!
//! A throwing `new` that cannot allocate must call the program's new-handler until it can, and
//! throw `std::bad_alloc` when there is none. The handler may throw too. Rust cannot throw a C++
//! exception, and one must not unwind through a Rust frame, so on x86-64 and AArch64 the throwing
//! forms are short assembly functions: they call the allocator and return its block, and when it
//! has none they jump to the C++ runtime's own definition of the operator, found with
//! `dlsym(RTLD_NEXT)`. That definition calls `malloc` or `aligned_alloc` of this library again,
//! runs the handler and throws, and the jump has left no frame of this library on the stack. On
//! other architectures the throwing forms are not defined, so the runtime's run instead. A
//! `nothrow` form that cannot allocate hands over to the runtime's `nothrow` form the same way,
//! which runs the handler and returns null.
//!
//! The sizes that the sized forms of `delete` pass are not needed: a free reads the block's size
//! class from the chunk map anyway.
//!
//! A program that replaces some of the operators itself gets this library's for the others, as with
//! the preload libraries of mimalloc, jemalloc and tcmalloc. The standard has the default `nothrow`
//! `new` call the program's `new`, and a sized `delete` its unsized `delete`; these do not.

#![allow(non_snake_case, reason = "these are the C++ ABI names of the operators")]

use crate::{ALLOCATOR, allocate};
use core::{
    ffi::{CStr, c_void},
    mem,
    ptr::{self, NonNull},
};

/// The forms of `new` that hand a failed allocation over to the C++ runtime, as indices into
/// [`RUNTIME_NAMES`].
const NEW: usize = 0;
const NEW_ARRAY: usize = 1;
const NEW_ALIGNED: usize = 2;
const NEW_ARRAY_ALIGNED: usize = 3;
const NEW_NOTHROW: usize = 4;
const NEW_ARRAY_NOTHROW: usize = 5;
const NEW_ALIGNED_NOTHROW: usize = 6;
const NEW_ARRAY_ALIGNED_NOTHROW: usize = 7;

/// The symbols of those forms.
const RUNTIME_NAMES: [&CStr; 8] = [
    c"_Znwm",
    c"_Znam",
    c"_ZnwmSt11align_val_t",
    c"_ZnamSt11align_val_t",
    c"_ZnwmRKSt9nothrow_t",
    c"_ZnamRKSt9nothrow_t",
    c"_ZnwmSt11align_val_tRKSt9nothrow_t",
    c"_ZnamSt11align_val_tRKSt9nothrow_t",
];

/// The C++ runtime's definition of the form of `new` at `form` in [`RUNTIME_NAMES`]: the next
/// one after this library's, or null if there is none.
fn runtime_new(form: usize) -> *mut c_void {
    // SAFETY: the name is a valid C string, and RTLD_NEXT searches the objects after this one.
    unsafe { libc::dlsym(libc::RTLD_NEXT, RUNTIME_NAMES[form].as_ptr()) }
}

/// Allocates for a throwing `new` of the default alignment, or returns null. Called from the
/// assembly of the throwing forms.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
extern "C" fn new_block(size: usize) -> *mut c_void {
    ALLOCATOR.malloc(size).cast()
}

/// Allocates for a throwing `new` with an alignment, or returns null. Called from the assembly of
/// the throwing forms.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
extern "C" fn new_aligned_block(size: usize, align: usize) -> *mut c_void {
    allocate(size, align)
}

/// Returns the C++ runtime's definition of the throwing form of `new` at `form`, for its caller
/// to jump to with the original arguments, or aborts if there is none. Called from the assembly of
/// the throwing forms once the allocator has failed.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
extern "C" fn runtime_throwing_new(form: usize) -> *mut c_void {
    let runtime = runtime_new(form);
    if runtime.is_null() {
        const MESSAGE: &[u8] = b"bcmalloc: out of memory in operator new, and no C++ runtime\n";
        // SAFETY: the message is valid for its length, and abort has no preconditions.
        unsafe {
            libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len());
            libc::abort()
        }
    }
    runtime
}

/// The body of a throwing `new` that takes a size. It saves the size across the call to `$block`,
/// and when that returns null it fetches the runtime's operator and jumps to it with the size, with
/// the stack as the caller left it.
#[cfg(target_arch = "x86_64")]
macro_rules! throwing_new {
    ($block:path, $form:expr) => {
        core::arch::naked_asm!(
            ".cfi_startproc",
            "push rdi",
            ".cfi_adjust_cfa_offset 8",
            "call {block}",
            "pop rdi",
            ".cfi_adjust_cfa_offset -8",
            "test rax, rax",
            "jz 2f",
            "ret",
            "2:",
            "push rdi",
            ".cfi_adjust_cfa_offset 8",
            "mov edi, {form}",
            "call {runtime}",
            "pop rdi",
            ".cfi_adjust_cfa_offset -8",
            "jmp rax",
            ".cfi_endproc",
            block = sym $block,
            runtime = sym runtime_throwing_new,
            form = const $form,
        )
    };
}

/// As [`throwing_new`] for a throwing `new` that takes a size and an alignment.
#[cfg(target_arch = "x86_64")]
macro_rules! throwing_new_aligned {
    ($block:path, $form:expr) => {
        core::arch::naked_asm!(
            ".cfi_startproc",
            "push rdi",
            ".cfi_adjust_cfa_offset 8",
            "push rsi",
            ".cfi_adjust_cfa_offset 8",
            "sub rsp, 8",
            ".cfi_adjust_cfa_offset 8",
            "call {block}",
            "add rsp, 8",
            ".cfi_adjust_cfa_offset -8",
            "pop rsi",
            ".cfi_adjust_cfa_offset -8",
            "pop rdi",
            ".cfi_adjust_cfa_offset -8",
            "test rax, rax",
            "jz 2f",
            "ret",
            "2:",
            "push rdi",
            ".cfi_adjust_cfa_offset 8",
            "push rsi",
            ".cfi_adjust_cfa_offset 8",
            "sub rsp, 8",
            ".cfi_adjust_cfa_offset 8",
            "mov edi, {form}",
            "call {runtime}",
            "add rsp, 8",
            ".cfi_adjust_cfa_offset -8",
            "pop rsi",
            ".cfi_adjust_cfa_offset -8",
            "pop rdi",
            ".cfi_adjust_cfa_offset -8",
            "jmp rax",
            ".cfi_endproc",
            block = sym $block,
            runtime = sym runtime_throwing_new,
            form = const $form,
        )
    };
}

/// The body of a throwing `new` that takes a size, as for x86-64. The runtime's operator is
/// reached through x16, which branch target identification accepts at a function's entry.
#[cfg(target_arch = "aarch64")]
macro_rules! throwing_new {
    ($block:path, $form:expr) => {
        core::arch::naked_asm!(
            ".cfi_startproc",
            "stp x0, x30, [sp, #-16]!",
            ".cfi_def_cfa_offset 16",
            ".cfi_offset x30, -8",
            "bl {block}",
            "ldp x1, x30, [sp], #16",
            ".cfi_def_cfa_offset 0",
            ".cfi_restore x30",
            "cbz x0, 2f",
            "ret",
            "2:",
            "stp x1, x30, [sp, #-16]!",
            ".cfi_def_cfa_offset 16",
            ".cfi_offset x30, -8",
            "mov x0, #{form}",
            "bl {runtime}",
            "mov x16, x0",
            "ldp x0, x30, [sp], #16",
            ".cfi_def_cfa_offset 0",
            ".cfi_restore x30",
            "br x16",
            ".cfi_endproc",
            block = sym $block,
            runtime = sym runtime_throwing_new,
            form = const $form,
        )
    };
}

/// As [`throwing_new`] for a throwing `new` that takes a size and an alignment.
#[cfg(target_arch = "aarch64")]
macro_rules! throwing_new_aligned {
    ($block:path, $form:expr) => {
        core::arch::naked_asm!(
            ".cfi_startproc",
            "stp x0, x1, [sp, #-32]!",
            ".cfi_def_cfa_offset 32",
            "str x30, [sp, #16]",
            ".cfi_offset x30, -16",
            "bl {block}",
            "ldr x30, [sp, #16]",
            "ldp x2, x3, [sp], #32",
            ".cfi_def_cfa_offset 0",
            ".cfi_restore x30",
            "cbz x0, 2f",
            "ret",
            "2:",
            "stp x2, x3, [sp, #-32]!",
            ".cfi_def_cfa_offset 32",
            "str x30, [sp, #16]",
            ".cfi_offset x30, -16",
            "mov x0, #{form}",
            "bl {runtime}",
            "mov x16, x0",
            "ldr x30, [sp, #16]",
            "ldp x0, x1, [sp], #32",
            ".cfi_def_cfa_offset 0",
            ".cfi_restore x30",
            "br x16",
            ".cfi_endproc",
            block = sym $block,
            runtime = sym runtime_throwing_new,
            form = const $form,
        )
    };
}

/// Hands a `nothrow` form of `new` that could not allocate over to the runtime's, which runs the
/// new-handler as the standard asks, and returns its result, or null if there is no runtime.
#[cold]
#[inline(never)]
fn runtime_nothrow_new(form: usize, size: usize) -> *mut c_void {
    let runtime = runtime_new(form);
    if runtime.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the symbol is the runtime's form at `form`, which takes a size and a nothrow_t, an
    // empty tag that is never read. A nothrow form catches every exception, so none unwinds into
    // this frame.
    unsafe {
        let new: unsafe extern "C" fn(usize, *const c_void) -> *mut c_void =
            mem::transmute(runtime);
        new(size, ptr::null())
    }
}

/// As [`runtime_nothrow_new`] for a `nothrow` form with an alignment.
#[cold]
#[inline(never)]
fn runtime_nothrow_new_aligned(form: usize, size: usize, align: usize) -> *mut c_void {
    let runtime = runtime_new(form);
    if runtime.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: as above, for a form that also takes the alignment.
    unsafe {
        let new: unsafe extern "C" fn(usize, usize, *const c_void) -> *mut c_void =
            mem::transmute(runtime);
        new(size, align, ptr::null())
    }
}

/// Frees for every form of `delete`.
#[inline(always)]
fn delete(ptr: *mut c_void) {
    if let Some(block) = NonNull::new(ptr.cast::<u8>()) {
        // SAFETY: `delete` is only given blocks that `new` returned, or null.
        unsafe { ALLOCATOR.free_unsized(block) };
    }
}

/// `operator new(size_t)`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[unsafe(naked)]
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _Znwm(size: usize) -> *mut c_void {
    throwing_new!(new_block, NEW)
}

/// `operator new[](size_t)`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[unsafe(naked)]
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _Znam(size: usize) -> *mut c_void {
    throwing_new!(new_block, NEW_ARRAY)
}

/// `operator new(size_t, const std::nothrow_t&)`.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnwmRKSt9nothrow_t(size: usize, _: *const c_void) -> *mut c_void {
    let block = ALLOCATOR.malloc(size).cast::<c_void>();
    if block.is_null() {
        return runtime_nothrow_new(NEW_NOTHROW, size);
    }
    block
}

/// `operator new[](size_t, const std::nothrow_t&)`.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnamRKSt9nothrow_t(size: usize, _: *const c_void) -> *mut c_void {
    let block = ALLOCATOR.malloc(size).cast::<c_void>();
    if block.is_null() {
        return runtime_nothrow_new(NEW_ARRAY_NOTHROW, size);
    }
    block
}

/// `operator new(size_t, std::align_val_t)`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[unsafe(naked)]
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnwmSt11align_val_t(size: usize, align: usize) -> *mut c_void {
    throwing_new_aligned!(new_aligned_block, NEW_ALIGNED)
}

/// `operator new[](size_t, std::align_val_t)`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[unsafe(naked)]
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnamSt11align_val_t(size: usize, align: usize) -> *mut c_void {
    throwing_new_aligned!(new_aligned_block, NEW_ARRAY_ALIGNED)
}

/// `operator new(size_t, std::align_val_t, const std::nothrow_t&)`.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnwmSt11align_val_tRKSt9nothrow_t(
    size: usize,
    align: usize,
    _: *const c_void,
) -> *mut c_void {
    let block = allocate(size, align);
    if block.is_null() {
        return runtime_nothrow_new_aligned(NEW_ALIGNED_NOTHROW, size, align);
    }
    block
}

/// `operator new[](size_t, std::align_val_t, const std::nothrow_t&)`.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub extern "C" fn _ZnamSt11align_val_tRKSt9nothrow_t(
    size: usize,
    align: usize,
    _: *const c_void,
) -> *mut c_void {
    let block = allocate(size, align);
    if block.is_null() {
        return runtime_nothrow_new_aligned(NEW_ARRAY_ALIGNED_NOTHROW, size, align);
    }
    block
}

/// `operator delete(void*)`.
///
/// # Safety
///
/// `ptr` must be null or a live block from `new`.
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPv(ptr: *mut c_void) {
    delete(ptr);
}

/// `operator delete[](void*)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPv(ptr: *mut c_void) {
    delete(ptr);
}

/// `operator delete(void*, size_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPvm(ptr: *mut c_void, _: usize) {
    delete(ptr);
}

/// `operator delete[](void*, size_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPvm(ptr: *mut c_void, _: usize) {
    delete(ptr);
}

/// `operator delete(void*, const std::nothrow_t&)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPvRKSt9nothrow_t(ptr: *mut c_void, _: *const c_void) {
    delete(ptr);
}

/// `operator delete[](void*, const std::nothrow_t&)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPvRKSt9nothrow_t(ptr: *mut c_void, _: *const c_void) {
    delete(ptr);
}

/// `operator delete(void*, std::align_val_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPvSt11align_val_t(ptr: *mut c_void, _: usize) {
    delete(ptr);
}

/// `operator delete[](void*, std::align_val_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPvSt11align_val_t(ptr: *mut c_void, _: usize) {
    delete(ptr);
}

/// `operator delete(void*, size_t, std::align_val_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPvmSt11align_val_t(ptr: *mut c_void, _: usize, _: usize) {
    delete(ptr);
}

/// `operator delete[](void*, size_t, std::align_val_t)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPvmSt11align_val_t(ptr: *mut c_void, _: usize, _: usize) {
    delete(ptr);
}

/// `operator delete(void*, std::align_val_t, const std::nothrow_t&)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdlPvSt11align_val_tRKSt9nothrow_t(
    ptr: *mut c_void,
    _: usize,
    _: *const c_void,
) {
    delete(ptr);
}

/// `operator delete[](void*, std::align_val_t, const std::nothrow_t&)`.
///
/// # Safety
///
/// As for [`_ZdlPv`].
#[cfg_attr(not(test), unsafe(no_mangle))]
pub unsafe extern "C" fn _ZdaPvSt11align_val_tRKSt9nothrow_t(
    ptr: *mut c_void,
    _: usize,
    _: *const c_void,
) {
    delete(ptr);
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{MIN_ALIGN, malloc_usable_size};
    use core::ptr;

    #[test]
    fn every_new_pairs_with_every_delete() {
        let nothrow = ptr::null::<c_void>();
        // SAFETY: each block comes from the matching new and is deleted once.
        unsafe {
            let plain = _Znwm(24);
            assert!(!plain.is_null() && plain.addr().is_multiple_of(MIN_ALIGN));
            _ZdlPvm(plain, 24);
            let array = _Znam(1000);
            assert!(malloc_usable_size(array) >= 1000);
            _ZdaPv(array);
            let quiet = _ZnwmRKSt9nothrow_t(100, nothrow);
            _ZdlPvRKSt9nothrow_t(quiet, nothrow);
            let quiet_array = _ZnamRKSt9nothrow_t(100, nothrow);
            _ZdaPvRKSt9nothrow_t(quiet_array, nothrow);
            for align in [32, 256, 4096] {
                let aligned = _ZnwmSt11align_val_t(40, align);
                assert!(aligned.addr().is_multiple_of(align), "aligned to {align}");
                _ZdlPvmSt11align_val_t(aligned, 40, align);
                let aligned = _ZnamSt11align_val_t(40, align);
                assert!(aligned.addr().is_multiple_of(align));
                _ZdaPvSt11align_val_t(aligned, align);
                let aligned = _ZnwmSt11align_val_tRKSt9nothrow_t(40, align, nothrow);
                _ZdlPvSt11align_val_tRKSt9nothrow_t(aligned, align, nothrow);
                let aligned = _ZnamSt11align_val_tRKSt9nothrow_t(40, align, nothrow);
                _ZdaPvSt11align_val_tRKSt9nothrow_t(aligned, align, nothrow);
            }
            _ZdlPv(ptr::null_mut());
            _ZdaPvm(ptr::null_mut(), 0);
            _ZdlPvSt11align_val_t(ptr::null_mut(), 64);
            _ZdaPvmSt11align_val_t(ptr::null_mut(), 0, 64);
        }
    }

    #[test]
    fn nothrow_new_returns_null_when_it_cannot_allocate() {
        assert!(_ZnwmRKSt9nothrow_t(usize::MAX, ptr::null()).is_null());
        assert!(_ZnamSt11align_val_tRKSt9nothrow_t(usize::MAX / 2, 64, ptr::null()).is_null());
    }
}
