set positional-arguments

# Show the available recipes.
default:
    @just --list

# Build everything.
build *args='':
    cargo build --all-targets --all-features $@

# Format the crate.
fmt:
    cargo +nightly fmt --all

# Check formatting.
fmt-check:
    cargo +nightly fmt --all -- --check

# Check formatting, then run clippy on every target and feature, and on the library as loom builds it.
lint: fmt-check
    cargo +nightly clippy --all-targets --all-features -- -D warnings
    RUSTFLAGS="--cfg loom" cargo +nightly clippy --profile test --lib -- -D warnings

# Build the rustdoc with warnings denied, private items included.
lint-docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --all-features

# Run the doctests.
test-docs:
    cargo test --doc --all-features

# Run the unit tests, the integration tests and the doctests.
test *args='':
    cargo nextest run --all-features $@
    cargo test --doc --all-features

# Check the library under every feature combination. The tests and benchmarks need `std`.
hack *args='':
    cargo hack --feature-powerset check --lib $@

# Run the tests under Miri with strict provenance.
miri *args='':
    MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test --all-features --lib $@

# Model check the lock-free code with loom.
loom *args='':
    RUSTFLAGS="--cfg loom" cargo test --release --lib loom_test $@

# Fuzz one target for a number of seconds. The targets are heap_ops and chunk_ops.
fuzz target='heap_ops' seconds='60':
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Build the benchmarks without running them.
bench-build:
    cargo bench --no-run

# Run every allocator benchmark and print one comparison table. Pass --quick for a short run.
bench *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target
    : > target/bench.tsv
    for allocator in system cwmalloc mimalloc jemalloc snmalloc; do
        cargo bench --bench "$allocator" -- "$@" >> target/bench.tsv
    done
    cargo bench --bench report -- target/bench.tsv 2>/dev/null
