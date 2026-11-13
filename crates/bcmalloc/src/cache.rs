//! The cache line geometry that the metadata layouts are built around.
//!
//! Two threads that write to the same cache line slow each other down even when they touch
//! different bytes, because the line has to move between their cores. Descriptors and heap slots
//! therefore keep the state that other threads write on lines of its own, away from the state that
//! only the owner touches. Some processors fetch lines in aligned pairs and some, such as Apple
//! silicon, have lines that are 128 bytes long, so those targets are padded to 128 bytes.

/// A zero-sized field that raises the alignment of its struct to a whole cache line.
///
/// Put `_line: [CacheLine; 0]` first in a struct to start it on a fresh line and to round its size
/// up to a whole number of lines.
#[cfg_attr(
    any(target_arch = "aarch64", target_arch = "x86_64", target_arch = "powerpc64"),
    repr(align(128))
)]
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64", target_arch = "powerpc64")),
    repr(align(64))
)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CacheLine;
