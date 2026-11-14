set positional-arguments

# Show the available recipes.
default:
    @just --list

# Build everything in the workspace.
build *args:
    cargo build --workspace --all-targets --all-features "$@"

# Format the workspace and the fuzz crate.
fmt:
    cargo +nightly fmt --all
    cargo +nightly fmt --all --manifest-path fuzz/Cargo.toml

# Check formatting.
fmt-check:
    cargo +nightly fmt --all -- --check
    cargo +nightly fmt --all --manifest-path fuzz/Cargo.toml -- --check

# Check formatting, then run clippy on every crate, target and feature, and on the library as loom
# builds it.
lint: fmt-check
    cargo +nightly clippy --workspace --all-targets --all-features -- -D warnings
    RUSTFLAGS="--cfg loom" cargo +nightly clippy -p bcmalloc --profile test --lib -- -D warnings

# Build the rustdoc with warnings denied, private items included.
lint-docs:
    RUSTDOCFLAGS="-D warnings" cargo doc -p bcmalloc --no-deps --document-private-items --all-features

# Run the doctests.
test-docs:
    cargo test -p bcmalloc --doc --all-features

# Run the unit tests, the integration tests and the doctests.
test *args:
    cargo nextest run -p bcmalloc -p bcmalloc-cabi --all-features "$@"
    cargo test -p bcmalloc --doc --all-features

# Check the library under every feature combination. The tests and benchmarks need `std`.
hack *args:
    cargo hack -p bcmalloc --feature-powerset check --lib "$@"

# Run the tests under Miri with strict provenance.
miri *args:
    MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri nextest run -p bcmalloc --all-features --lib "$@"

# Measure the line coverage of the library's tests, with every feature, into lcov.info. Needs
# cargo-llvm-cov and the llvm-tools-preview component.
coverage *args:
    cargo llvm-cov nextest -p bcmalloc -p bcmalloc-cabi --all-features --lcov --output-path lcov.info "$@"

# Model check the lock-free code with loom.
loom *args:
    RUSTFLAGS="--cfg loom" cargo nextest run -p bcmalloc --release --lib loom_test "$@"

# Fuzz one target for a number of seconds. The targets are heap_ops and chunk_ops.
fuzz target='heap_ops' seconds='60':
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Build the benchmarks and the held-out programs without running them.
bench-build:
    cargo bench -p bcmalloc-bench -p bcmalloc-bench-apps --no-run

# Run every workload under every allocator and print the comparison with medians and bootstrap
# intervals. Options are --quick, --runs N (default 10), --filter TEXT, --only A,B, --baseline
# ALLOCATOR, --cpus LIST (taskset, Linux), --manifest PATH (the benches of another crate), --save
# PATH, --load PATH (report a saved file instead of running) and --perf, which counts hardware
# events with perf stat on Linux. Every sample goes to target/bench.tsv, and a complete run is
# recorded in benchmarks.json, which renders BENCHMARKS.md, README.md and the figure again.
bench *args:
    cargo bench -p bcmalloc-bench --bench report -- "$@"

# Compare two saved sample files from bench --save and print the change of each median with a 95%
# interval. Only bcmalloc is shown unless --only A,B says otherwise.
bench-compare old new *args:
    old="$1"; new="$2"; shift 2; cargo bench -p bcmalloc-bench --bench report -- --compare "$old" "$new" "$@"

# Run the held-out programs in crates/bench-apps (serde_json, regex, syn, a hyper server, tokio
# channels) with the same driver and options as bench. They are never tuned against.
bench-apps *args:
    cargo bench -p bcmalloc-bench --bench report -- --manifest crates/bench-apps/Cargo.toml "$@"

# Build the C allocation interface as target/cabi/release/libbcmalloc_c.so (or .dylib), to preload
# into C programs. The initial-exec TLS model needs nightly, and is what a preloaded library wants.
# It builds in its own target directory, so the flag does not rebuild the rest of the workspace.
cabi:
    RUSTFLAGS="-Z tls-model=initial-exec" cargo +nightly build -p bcmalloc-cabi --release --target-dir target/cabi

# Run mimalloc-bench (https://github.com/daanx/mimalloc-bench, built with its build-bench-env.sh)
# with bcmalloc added as an external allocator, for example
# just mimalloc-bench ~/mimalloc-bench "sys bc mi je sn tc allt -r=3".
mimalloc-bench dir args: cabi
    echo "bc $PWD/target/cabi/release/libbcmalloc_c.$(if [ "$(uname)" = Darwin ]; then echo dylib; else echo so; fi)" > target/bcmalloc-external.txt
    cd "$1/out/bench" && ../../bench.sh --external="$OLDPWD/target/bcmalloc-external.txt" $2
    cargo run -q -p bcmalloc-bench-data -- import-mimalloc-bench "$1/out/bench/benchres.csv"

# Run the programs of mimalloc-bench, built in DIR by its build-bench-env.sh, under bcmalloc and the
# allocators named with --alloc, with wall time in nanoseconds, the allocators interleaved, short
# programs repeated, and medians with 95% intervals. See scripts/mbrun.py for the options, for
# example: just mbrun ~/mimalloc-bench --alloc mi --alloc sn --runs 10 --out target/mb.tsv
mbrun dir *args: cabi
    dir="$1"; shift; scripts/mbrun.py --dir "$dir" --alloc bc=target/cabi/release/libbcmalloc_c.so "$@"

# Render the tables of BENCHMARKS.md and README.md and assets/benchmarks.svg from benchmarks.json.
# The benchmark recipes do this after they record their results.
bench-render:
    cargo run -q -p bcmalloc-bench-data -- render

# Fail if the rendered tables or figure differ from what benchmarks.json gives.
bench-check:
    cargo run -q -p bcmalloc-bench-data -- check
