//! The atomics the allocator is written against, and the one way it waits for another thread.
//!
//! Normal builds use `core`'s atomics. Under `--cfg loom` the same names resolve to loom's
//! instrumented versions, so the model checker can explore every interleaving of the lock-free
//! code without that code knowing the difference.

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(loom)]
pub(crate) use loom::thread::yield_now;

/// Lets other threads run, for a thread that waits on one of them.
#[cfg(all(not(loom), any(test, feature = "std")))]
pub(crate) fn yield_now() {
    // SAFETY: sched_yield has no preconditions, and its only failure is not yielding.
    unsafe { libc::sched_yield() };
}
