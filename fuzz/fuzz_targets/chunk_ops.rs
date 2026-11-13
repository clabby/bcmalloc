//! Feeds arbitrary chunk allocator operations to its reference model.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| cwmalloc::testing::run_chunk_ops(data));
