//! The atomics the allocator is written against.
//!
//! Normal builds use `core`'s atomics. Under `--cfg loom` the same names resolve to loom's
//! instrumented versions, so the model checker can explore every interleaving of the lock-free
//! code without that code knowing the difference.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};
