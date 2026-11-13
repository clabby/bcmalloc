//! bcmalloc with its default configuration.

mod common;

#[global_allocator]
static GLOBAL: bcmalloc::Bcmalloc = bcmalloc::Bcmalloc::new();

fn main() {
    common::run("bcmalloc");
}
