//! jemalloc through tikv-jemallocator.

mod common;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    common::run("jemalloc");
}
