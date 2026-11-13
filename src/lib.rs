#![doc = include_str!("../README.md")]
#![no_std]

#[cfg(any(test, feature = "std"))]
#[macro_use]
extern crate std;

mod cache;
mod chunks;
mod config;
#[cfg(all(feature = "std", not(loom)))]
mod global;
mod heap;
mod region;
mod size_class;
mod slab;
mod sync;
mod sys;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(all(feature = "std", not(loom)))]
mod tls;

pub use config::{Commit, Config};
#[cfg(all(feature = "std", not(loom)))]
pub use global::Bcmalloc;
pub use heap::Heap;
pub use region::{Error, Mapping, Region, Stats};
pub use size_class::{MAX_ALIGN, MAX_SMALL};
