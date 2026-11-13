//! Feeds arbitrary allocator operations across three heaps to the model checker.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| cwmalloc::testing::run_ops(data));
