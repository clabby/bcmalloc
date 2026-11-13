//! cwmalloc with its default configuration.

mod common;

#[global_allocator]
static GLOBAL: cwmalloc::Cwmalloc = cwmalloc::Cwmalloc::new();

fn main() {
    common::run("cwmalloc");
}
