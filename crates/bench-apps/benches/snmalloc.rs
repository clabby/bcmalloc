//! snmalloc.

mod common;

#[global_allocator]
static GLOBAL: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

fn main() {
    common::run("snmalloc");
}
