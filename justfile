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
    MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri nextest run --all-features --lib $@

# Model check the lock-free code with loom.
loom *args='':
    RUSTFLAGS="--cfg loom" cargo nextest run --release --lib loom_test $@

# Fuzz one target for a number of seconds. The targets are heap_ops and chunk_ops.
fuzz target='heap_ops' seconds='60':
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Build the benchmarks without running them.
bench-build:
    cargo bench --no-run

# Run every workload under every allocator and print the comparison with medians and bootstrap
# intervals. Options are --quick, --runs N (default 10), --filter TEXT, --only A,B, --baseline
# ALLOCATOR, --cpus LIST (taskset, Linux), --manifest PATH (the benches of another crate), --save
# PATH and --perf, which counts hardware events with perf stat on Linux. Every sample goes to
# target/bench.tsv.
bench *args='':
    cargo bench --bench report -- $@

# Compare two saved sample files from bench --save and print the change of each median with a 95%
# interval. Only bcmalloc is shown unless --only A,B says otherwise.
bench-compare old new *args='':
    old="$1"; new="$2"; shift 2; cargo bench --bench report -- --compare "$old" "$new" "$@"
