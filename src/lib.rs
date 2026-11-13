#![doc = include_str!("../README.md")]
#![no_std]

#[cfg(any(test, feature = "std"))]
#[macro_use]
extern crate std;

mod chunks;
mod sync;
mod sys;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
