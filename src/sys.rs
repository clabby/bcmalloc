//! Operating system services for reserving, committing and discarding address space.
//!
//! The allocator manages one large mapping that it reserves at start-up. Everything it needs from
//! the OS is a handful of calls on that mapping, and Linux and macOS differ in only a few of them.
//!
//! | Operation       | Linux                             | macOS                              |
//! | --------------- | --------------------------------- | ---------------------------------- |
//! | [`reserve`]     | `mmap` with `MAP_NORESERVE`       | `mmap` with `MAP_NORESERVE`        |
//! | [`advise_huge`] | `madvise(MADV_HUGEPAGE)`          | nothing, there is no THP           |
//! | [`populate`]    | `MADV_POPULATE_WRITE`, else touch | touch every page                   |
//! | [`lock`]        | `mlock`                           | `mlock`                            |
//! | [`discard`]     | `MADV_DONTNEED`, reads as zeros   | `MADV_FREE_REUSABLE`, not zeroed   |
//! | [`reuse`]       | nothing                           | `MADV_FREE_REUSE`                  |
//!
//! A reservation is ordinary read-write anonymous memory that the kernel only backs with physical
//! pages when they are first touched. Reserving tens of gigabytes therefore costs address space and
//! nothing else. Under Miri only the reservation exists and every hint does nothing.

use core::ptr::NonNull;

#[cfg(not(unix))]
compile_error!("cwmalloc supports Linux and macOS only");

/// Reserves `len` bytes of zeroed, lazily committed, read-write memory.
///
/// The mapping is page aligned. Returns `None` if the kernel refuses, for example because the
/// address space or an overcommit limit is exhausted.
pub(crate) fn reserve(len: usize) -> Option<NonNull<u8>> {
    #[allow(unused_mut)]
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    // The kernel must not charge the whole reservation against the commit limit.
    #[cfg(not(miri))]
    {
        flags |= libc::MAP_NORESERVE;
    }
    // SAFETY: an anonymous mapping at a kernel-chosen address cannot alias existing memory.
    let ptr = unsafe {
        libc::mmap(core::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0)
    };
    if ptr == libc::MAP_FAILED { None } else { NonNull::new(ptr.cast()) }
}

/// Returns a reservation to the OS.
///
/// # Safety
///
/// `ptr` and `len` must be exactly the pair returned by and passed to [`reserve`], and nothing
/// may use the memory afterwards.
pub(crate) unsafe fn release(ptr: NonNull<u8>, len: usize) {
    // SAFETY: forwarded from the caller.
    let result = unsafe { libc::munmap(ptr.as_ptr().cast(), len) };
    debug_assert_eq!(result, 0, "munmap failed");
}

/// Asks the kernel to back the range with transparent huge pages where it can.
///
/// # Safety
///
/// The range must lie within a live reservation.
#[cfg_attr(
    not(all(target_os = "linux", not(miri))),
    allow(clippy::missing_const_for_fn, reason = "only Linux makes a system call here")
)]
pub(crate) unsafe fn advise_huge(ptr: NonNull<u8>, len: usize) {
    #[cfg(all(target_os = "linux", not(miri)))]
    // SAFETY: forwarded from the caller, and the advice never changes memory contents.
    unsafe {
        libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_HUGEPAGE);
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    let _ = (ptr, len);
}

/// Backs every page of the range with physical memory now instead of on first touch.
///
/// # Safety
///
/// The range must lie within a live reservation that holds nothing the caller needs, because the
/// fallback path overwrites the first byte of every page with zero.
#[cfg(not(miri))]
pub(crate) unsafe fn populate(ptr: NonNull<u8>, len: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: forwarded from the caller. The kernel rejects the advice before Linux 5.14.
    if unsafe { libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_POPULATE_WRITE) } == 0 {
        return;
    }
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    for offset in (0..len).step_by(page) {
        // SAFETY: the offset is inside the reservation, which is writable.
        unsafe { ptr.as_ptr().add(offset).write_volatile(0) };
    }
}

/// Backs every page of the range with physical memory now. Miri has no paging, so this does
/// nothing.
///
/// # Safety
///
/// Same contract as the non-Miri version.
#[cfg(miri)]
pub(crate) unsafe fn populate(_ptr: NonNull<u8>, _len: usize) {}

/// Pins the range in RAM so it is never swapped out. Returns whether the kernel agreed, which
/// depends on `RLIMIT_MEMLOCK`.
///
/// # Safety
///
/// The range must lie within a live reservation.
pub(crate) unsafe fn lock(ptr: NonNull<u8>, len: usize) -> bool {
    #[cfg(not(miri))]
    {
        // SAFETY: forwarded from the caller.
        unsafe { libc::mlock(ptr.as_ptr().cast(), len) == 0 }
    }
    #[cfg(miri)]
    {
        let _ = (ptr, len);
        false
    }
}

/// Tells the kernel the contents of the range are no longer needed, so it may reclaim the physical
/// pages. The range stays mapped and usable. Reading it afterwards returns zeros on Linux and
/// either the old contents or zeros on macOS.
///
/// # Safety
///
/// The range must lie within a live reservation, and no live allocation may overlap it.
pub(crate) unsafe fn discard(ptr: NonNull<u8>, len: usize) {
    #[cfg(not(miri))]
    {
        #[cfg(target_os = "linux")]
        const ADVICE: libc::c_int = libc::MADV_DONTNEED;
        // The reusable advice takes the pages out of the process's footprint at once, where the
        // plain free advice waits for memory pressure. It must be undone by [`reuse`].
        #[cfg(target_os = "macos")]
        const ADVICE: libc::c_int = libc::MADV_FREE_REUSABLE;
        // SAFETY: forwarded from the caller.
        unsafe { libc::madvise(ptr.as_ptr().cast(), len, ADVICE) };
    }
    #[cfg(miri)]
    let _ = (ptr, len);
}

/// Prepares pages that [`discard`] gave back for use again. Linux needs nothing, because the pages
/// come back zeroed on first touch. On macOS this restores the accounting of the pages.
///
/// # Safety
///
/// The range must lie within a live reservation and belong to the caller.
pub(crate) unsafe fn reuse(ptr: NonNull<u8>, len: usize) {
    #[cfg(all(target_os = "macos", not(miri)))]
    // SAFETY: forwarded from the caller.
    unsafe {
        libc::madvise(ptr.as_ptr().cast(), len, libc::MADV_FREE_REUSE);
    }
    #[cfg(not(all(target_os = "macos", not(miri))))]
    let _ = (ptr, len);
}

/// A monotonic clock in milliseconds.
pub(crate) fn monotonic_ms() -> u64 {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the pointer is valid for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000
}

#[cfg(test)]
mod test {
    use super::*;

    const LEN: usize = 1 << 20;

    fn reservation() -> NonNull<u8> {
        reserve(LEN).expect("a 1 MiB reservation always succeeds")
    }

    #[test]
    fn reservations_are_zeroed_and_writable() {
        let ptr = reservation();
        // SAFETY: the offsets are inside the live reservation.
        unsafe {
            assert_eq!(ptr.as_ptr().read(), 0);
            assert_eq!(ptr.as_ptr().add(LEN - 1).read(), 0);
            ptr.as_ptr().add(LEN / 2).write(7);
            assert_eq!(ptr.as_ptr().add(LEN / 2).read(), 7);
            release(ptr, LEN);
        }
    }

    #[test]
    fn populate_and_hints_leave_the_reservation_usable() {
        let ptr = reservation();
        // SAFETY: the range is a live, empty reservation.
        unsafe {
            advise_huge(ptr, LEN);
            populate(ptr, LEN);
            ptr.as_ptr().write(1);
            assert_eq!(ptr.as_ptr().read(), 1);
            release(ptr, LEN);
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", not(miri)))]
    fn discard_zero_fills_on_linux() {
        let ptr = reservation();
        // SAFETY: the range is a live reservation with no other users.
        unsafe {
            ptr.as_ptr().write(9);
            discard(ptr, LEN);
            assert_eq!(ptr.as_ptr().read(), 0);
            release(ptr, LEN);
        }
    }

    #[test]
    #[cfg(not(miri))]
    fn discard_keeps_the_range_writable() {
        let ptr = reservation();
        // SAFETY: the range is a live reservation with no other users.
        unsafe {
            ptr.as_ptr().write(9);
            discard(ptr, LEN);
            ptr.as_ptr().write(3);
            assert_eq!(ptr.as_ptr().read(), 3);
            release(ptr, LEN);
        }
    }
}
