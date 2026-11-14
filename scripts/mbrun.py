#!/usr/bin/env python3
"""Runs the programs of mimalloc-bench under several allocators, with precise measurements.

mimalloc-bench's own driver (bench.sh) times each program with GNU time, which reports wall time
in hundredths of a second, and it runs the allocators of a program one after another in a fixed
order. Many of its programs finish in well under a second, so one tick of that clock is several
percent of a result. This driver runs the same programs with the same arguments, built by
mimalloc-bench's own build script, and measures them differently:

* wall time in nanoseconds, and user time, system time, peak resident memory and page faults from
  wait4, for every run;
* every allocator takes a turn within each run, in an order that rotates from run to run and
  program to program, so that drift in the machine hits all of them alike;
* every child gets an environment variable of random length, which moves the initial stack
  (Mytkowicz et al., ASPLOS 2009);
* programs that finish in a fraction of a second run several times back to back within one sample
  in the default 'long' profile, which keeps the workload as it is and makes it long enough to
  time. '--profile stock' runs each once, as bench.sh does;
* the programs that report their own throughput (larson, rptest, xmalloc-test, glibc-thread and
  redis) are scored by it, converted to a relative time exactly as bench.sh converts it.

With --perf, every run is wrapped in perf stat and the counters are recorded too.

The summary shows the median of each program and allocator with a 95% bootstrap interval, and the
geometric mean of the ratios to a baseline allocator. A best result is starred when its interval
is clear of every other allocator's.

Usage, on the machine that has mimalloc-bench built in ~/mimalloc-bench:

    scripts/mbrun.py --alloc bc=target/cabi/release/libbcmalloc_c.so --alloc mi --alloc sn \\
        --runs 10 --out results.tsv
    scripts/mbrun.py --load results.tsv            # print the summary of a saved file again
    scripts/mbrun.py --load results.tsv --csv benchres.csv   # for bench-data import-mimalloc-bench
"""

import argparse
import math
import os
import random
import re
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

PROCS = os.cpu_count() or 1

# Allocators that mimalloc-bench builds, by the short names it uses, relative to its extern
# directory. mi2 is not part of mimalloc-bench and is built next to it by hand.
KNOWN_ALLOCATORS = {
    "mi": "mi/out/release/libmimalloc.so",
    "mi2": "mi2/out/release/libmimalloc.so",
    "mi1": "mi-1.8.2/out/release/libmimalloc.so",
    "sn": "sn/release/libsnmallocshim.so",
    "je": "je/lib/libjemalloc.so",
    "tc": "tc/.libs/libtcmalloc_minimal.so",
    "sys": None,
}

PERF_EVENTS = (
    "cycles,instructions,branch-misses,L1-dcache-load-misses,dTLB-load-misses,LLC-load-misses"
)

TSV_HEADER = [
    "test", "alloc", "run", "metric_s", "wall_s", "user_s", "sys_s", "maxrss_kib", "minflt",
    "majflt", "perf",
]


def relative_from(pattern, scale=None, last=False):
    """A parser that finds 'pattern' in a program's output. Its number is the relative time, or,
    with 'scale', a throughput that becomes scale / throughput, as bench.sh computes it."""

    def parse(text):
        found = re.findall(pattern, text, re.MULTILINE)
        if not found:
            return None
        value = float(found[-1] if last else found[0])
        if scale is None:
            return value
        return scale / value if value > 0 else None

    return parse


@dataclass
class Test:
    name: str
    cmd: list
    cwd: str = "out/bench"
    stdin: str = None
    # A command run before every sample, without the allocator, that is not timed.
    setup: list = None
    # How the program reports its result if not by its wall time.
    parse: object = None
    # How many times the program runs back to back in one sample of the long profile.
    reps: int = 1
    server: bool = False


def tests(root):
    ext = root / "extern"
    bench = root / "bench"
    p = str(PROCS)
    lean_jobs = "8" if PROCS > 8 else p
    linux = ext / "linux-6.6.105"
    redis = ext / "redis-6.2.7" / "src"
    return {
        t.name: t
        for t in [
            Test("cfrac", ["./cfrac", "17545186520507317056371138836327483792789528"]),
            Test("espresso", ["./espresso", str(bench / "espresso/largest.espresso")]),
            Test("barnes", ["./barnes"], stdin=str(bench / "barnes/input")),
            Test(
                "redis",
                [str(redis / "redis-benchmark"), "-r", "1000000", "-n", "100000", "-q", "-P",
                 "16", "lpush", "a", "1", "2", "3", "4", "5", "lrange", "a", "1", "5"],
                parse=relative_from(r"([0-9.]+) requests per second", 2_000_000, last=True),
                server=True,
            ),
            Test("larsonN", ["./larson", "5", "8", "1000", "5000", "100", "4141", p],
                 parse=relative_from(r" time: ([0-9.]+)")),
            Test("larsonN-sized", ["./larson-sized", "5", "8", "1000", "5000", "100", "4141", p],
                 parse=relative_from(r" time: ([0-9.]+)")),
            Test("mstressN", ["./mstress", p, "50", "25"], reps=2),
            Test("rptestN", ["./rptest", p, "0", "1", "2", "500", "1000", "100", "8", "16000"],
                 parse=relative_from(r"\.\.\.(\d+) memory ops", 2_000_000)),
            Test("gs", ["gs", "-dBATCH", "-dNODISPLAY", str(ext / "large.pdf")], reps=4),
            Test("alloc-test1", ["./alloc-test", "1"]),
            Test("alloc-testN", ["./alloc-test", str(min(PROCS, 16))]),
            Test("sh6benchN", ["./sh6bench", str(2 * PROCS)], reps=10),
            Test("sh8benchN", ["./sh8bench", str(2 * PROCS)], reps=5),
            Test("xmalloc-testN", ["./xmalloc-test", "-w", p, "-t", "5", "-s", "64"],
                 parse=relative_from(r"rtime: ([0-9.]+)")),
            Test("cache-scratch1", ["./cache-scratch", "1", "1000", "1", "2000000", p]),
            Test("cache-scratchN", ["./cache-scratch", p, "1000", "1", "2000000", p], reps=10),
            Test("cache-thrash1", ["./cache-thrash", "1", "1000", "1", "2000000", p]),
            Test("cache-thrashN", ["./cache-thrash", p, "1000", "1", "2000000", p], reps=10),
            Test("glibc-simple", ["./glibc-simple"]),
            Test("glibc-thread", ["./glibc-thread", p],
                 parse=relative_from(r"^([0-9.]+)", 1_000_000_000)),
            Test("rocksdb", [str(ext / "rocksdb-10.10.1/db_bench"), "--benchmarks=fillrandom"]),
            Test(
                "leanN", ["../bin/lean", "--make", "-j", lean_jobs], cwd=str(ext / "lean/library"),
                setup=["make", "-C", str(ext / "lean/out/release"), "clean-olean"],
            ),
            Test(
                "linux", ["make", "O=/tmp/linux_build", "-j", p], cwd=str(linux),
                setup=["make", "-C", str(linux), "O=/tmp/linux_build", "distclean", "allnoconfig",
                       "-j", p],
            ),
            Test("malloc-large", ["./malloc-large"]),
            Test("mleak10", ["./mleak", "5"], reps=5),
            Test("mleak100", ["./mleak", "50"]),
            Test("rbstress1", ["ruby", str(bench / "rbstress/stress_mem.rb"), "1"]),
            Test("rbstressN", ["ruby", str(bench / "rbstress/stress_mem.rb"), p]),
        ]
    }


@dataclass
class Sample:
    metric: float
    wall: float
    user: float
    sys: float
    maxrss: int
    minflt: int
    majflt: int
    perf: dict = field(default_factory=dict)


def timed(cmd, rss_out):
    """Runs a command under GNU time for its peak resident memory only. A child forked from this
    Python process would start out with the interpreter's resident pages, and the kernel counts
    those towards the child's peak, so the program is started from GNU time's small process, as
    bench.sh starts it."""
    return ["/usr/bin/time", "-f", "%M", "-o", rss_out] + cmd


def read_rss(path):
    try:
        lines = Path(path).read_text().split()
        os.unlink(path)
        return int(lines[-1])
    except (OSError, ValueError, IndexError):
        return 0


def child_env(lib, pad):
    env = dict(os.environ)
    env.pop("LD_PRELOAD", None)
    if lib:
        env["LD_PRELOAD"] = lib
    env["BCMALLOC_BENCH_PAD"] = "x" * pad
    return env


def wrap_perf(cmd, env, perf_out, args):
    """Wraps a command in perf stat, which needs privileges here, and runs the command itself as
    the calling user with exactly the environment it would have had."""
    if not args.perf:
        return cmd, env
    inner = ["setpriv", f"--reuid={os.getuid()}", f"--regid={os.getgid()}", "--init-groups",
             "env", "-i"] + [f"{k}={v}" for k, v in env.items()] + cmd
    outer = args.perf_cmd.split() + ["stat", "-x", ",", "-e", args.perf, "-o", perf_out, "--"]
    return outer + inner, {k: v for k, v in os.environ.items() if k != "LD_PRELOAD"}


SCRATCH = tempfile.mkdtemp(prefix="mbrun-")


def scratch(prefix):
    """A fresh file that this user owns, in a private directory. perf runs as root here, and Linux
    keeps root from opening another user's file in a shared sticky directory such as /tmp."""
    fd, path = tempfile.mkstemp(prefix=prefix, dir=SCRATCH)
    os.close(fd)
    return path


def read_perf(path):
    counters = {}
    try:
        lines = Path(path).read_text().splitlines()
    except OSError:
        return counters
    for line in lines:
        parts = line.split(",")
        if len(parts) >= 3 and parts[0] and not line.startswith("#"):
            try:
                counters[parts[2]] = counters.get(parts[2], 0) + float(parts[0])
            except ValueError:
                pass
    return counters


def wait_rusage(proc):
    _, status, usage = os.wait4(proc.pid, 0)
    proc.returncode = os.waitstatus_to_exitcode(status)
    return proc.returncode, usage


def sample_of(wall, usage, perf_out, rss_out):
    perf = read_perf(perf_out)
    if os.path.exists(perf_out):
        os.unlink(perf_out)
    return Sample(0.0, wall, usage.ru_utime, usage.ru_stime, read_rss(rss_out), usage.ru_minflt,
                  usage.ru_majflt, perf)


def run_server_test(test, root, lib, pad, args, out):
    """Runs redis as bench.sh does: the server under the allocator, and the benchmark client
    without it. The client's throughput is the result and the server's usage is recorded."""
    redis = root / "extern" / "redis-6.2.7" / "src"
    cli = [str(redis / "redis-cli")]
    perf_out = scratch("mbrun-perf-")
    rss_out = scratch("mbrun-rss-")
    server_cmd = timed([str(redis / "redis-server")], rss_out)
    cmd, env = wrap_perf(server_cmd, child_env(lib, pad), perf_out, args)
    server = subprocess.Popen(cmd, cwd=root / test.cwd, env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    clean = child_env(None, 0)
    for _ in range(600):
        if subprocess.run(cli + ["ping"], env=clean, capture_output=True).returncode == 0:
            break
        time.sleep(0.1)
    else:
        server.kill()
        wait_rusage(server)
        return None, "the redis server never accepted commands"
    start = time.perf_counter_ns()
    client = subprocess.run(test.cmd, cwd=root / test.cwd, env=clean, stdout=out,
                            stderr=subprocess.STDOUT)
    wall = (time.perf_counter_ns() - start) / 1e9
    subprocess.run(cli + ["flushall"], env=clean, capture_output=True)
    subprocess.run(cli + ["shutdown"], env=clean, capture_output=True)
    _, usage = wait_rusage(server)
    if client.returncode != 0:
        return None, f"the client exited with {client.returncode}"
    return sample_of(wall, usage, perf_out, rss_out), None


def run_once(test, root, lib, pad, args, out):
    if test.setup:
        subprocess.run(test.setup, cwd=root / test.cwd, env=child_env(None, 0),
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if test.server:
        return run_server_test(test, root, lib, pad, args, out)
    perf_out = scratch("mbrun-perf-")
    rss_out = scratch("mbrun-rss-")
    cmd, env = wrap_perf(timed(test.cmd, rss_out), child_env(lib, pad), perf_out, args)
    stdin = open(test.stdin, "rb") if test.stdin else subprocess.DEVNULL
    try:
        start = time.perf_counter_ns()
        proc = subprocess.Popen(cmd, cwd=root / test.cwd, env=env, stdin=stdin, stdout=out,
                                stderr=subprocess.STDOUT)
        code, usage = wait_rusage(proc)
        wall = (time.perf_counter_ns() - start) / 1e9
    finally:
        if test.stdin:
            stdin.close()
    if code != 0:
        return None, f"exited with {code}"
    return sample_of(wall, usage, perf_out, rss_out), None


def run_sample(test, root, lib, pad, args):
    """Runs a test 'reps' times back to back and returns one sample: the mean result, CPU times and
    faults per run, and the largest peak."""
    reps = test.reps if args.profile == "long" else 1
    total = None
    with tempfile.TemporaryFile() as out:
        for _ in range(reps):
            out.seek(0)
            out.truncate()
            sample, error = run_once(test, root, lib, pad, args, out)
            out.seek(0)
            text = out.read().decode(errors="replace")
            if sample is None:
                return None, f"{error}: {text[-400:]}"
            sample.metric = test.parse(text) if test.parse else sample.wall
            if sample.metric is None:
                return None, f"no result in the output: {text[-400:]}"
            if total is None:
                total = sample
                continue
            total.metric += sample.metric
            total.wall += sample.wall
            total.user += sample.user
            total.sys += sample.sys
            total.minflt += sample.minflt
            total.majflt += sample.majflt
            total.maxrss = max(total.maxrss, sample.maxrss)
            for k, v in sample.perf.items():
                total.perf[k] = total.perf.get(k, 0) + v
    for name in ("metric", "wall", "user", "sys"):
        setattr(total, name, getattr(total, name) / reps)
    total.minflt //= reps
    total.majflt //= reps
    total.perf = {k: v / reps for k, v in total.perf.items()}
    return total, None


def resolve_allocators(specs, root):
    allocators = []
    for spec in specs:
        name, _, path = spec.partition("=")
        if not path:
            if name not in KNOWN_ALLOCATORS:
                sys.exit(f"unknown allocator {name}; give it as NAME=PATH")
            rel = KNOWN_ALLOCATORS[name]
            path = str(root / "extern" / rel) if rel else ""
        else:
            path = str(Path(path).expanduser().resolve())
        if path and not os.path.exists(path):
            sys.exit(f"{name}: {path} does not exist")
        allocators.append((name, path))
    return allocators


# ---------------------------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------------------------


def median(values):
    values = sorted(values)
    n = len(values)
    return values[n // 2] if n % 2 else (values[n // 2 - 1] + values[n // 2]) / 2


def bootstrap(values, rng, resamples=2000):
    """A 95% bootstrap interval of the median."""
    if len(values) < 2:
        return values[0], values[0]
    medians = sorted(median(rng.choices(values, k=len(values))) for _ in range(resamples))
    return medians[int(0.025 * resamples)], medians[int(0.975 * resamples) - 1]


def load(path):
    with open(path) as f:
        header = f.readline().rstrip("\n").split("\t")
        return [dict(zip(header, line.rstrip("\n").split("\t"))) for line in f if line.strip()]


COLUMNS = {
    "metric_s": ("time (s)", 1.0),
    "maxrss_kib": ("peak RSS (MiB)", 1 / 1024),
    "user_s": ("user time (s)", 1.0),
    "sys_s": ("system time (s)", 1.0),
    "minflt": ("minor faults", 1.0),
}


def summarize(rows, baseline, columns):
    rng = random.Random(0xB007)
    if not rows:
        return "no samples"
    tests, allocs, data = [], [], {}
    for row in rows:
        if row["test"] not in tests:
            tests.append(row["test"])
        if row["alloc"] not in allocs:
            allocs.append(row["alloc"])
        data.setdefault((row["test"], row["alloc"]), []).append(row)
    if baseline not in allocs:
        baseline = allocs[0]
    out = []
    for column in columns:
        label, scale = COLUMNS[column]
        out.append(f"\n## {label}: median [95% interval]; bold is best, * if clear of all others\n")
        out.append("| test | " + " | ".join(allocs) + " |")
        out.append("|---" * (len(allocs) + 1) + "|")
        ratios = {a: [] for a in allocs}
        for test in tests:
            stats = {}
            for alloc in allocs:
                values = [float(r[column]) * scale for r in data.get((test, alloc), [])]
                if values:
                    stats[alloc] = (median(values), *bootstrap(values, rng))
            if not stats:
                continue
            best = min(stats, key=lambda a: stats[a][0])
            cells = []
            for alloc in allocs:
                if alloc not in stats:
                    cells.append("-")
                    continue
                med, lo, hi = stats[alloc]
                clear = alloc == best and len(stats) > 1 and all(
                    other == alloc or stats[other][1] > hi for other in stats)
                mark = "**" if alloc == best else ""
                cells.append(f"{mark}{med:.4g}{mark} [{lo:.4g}, {hi:.4g}]{' *' if clear else ''}")
                if baseline in stats and stats[baseline][0] > 0 and med > 0:
                    ratios[alloc].append(stats[baseline][0] / med)
            out.append(f"| {test} | " + " | ".join(cells) + " |")
        gm = [f"{math.exp(sum(map(math.log, r)) / len(r)):.3f}" if r else "-"
              for r in (ratios[a] for a in allocs)]
        out.append(f"| geomean, {baseline} / allocator | " + " | ".join(gm) + " |")
    return "\n".join(out)


def write_csv(rows, path):
    """Writes a benchres.csv in mimalloc-bench's format: test, allocator, time, peak KiB, user,
    sys, major and minor faults."""
    with open(path, "w") as f:
        for r in rows:
            f.write(f"{r['test']} {r['alloc']} {float(r['metric_s']):.4f} {r['maxrss_kib']} "
                    f"{float(r['user_s']):.3f} {float(r['sys_s']):.3f} {r['majflt']} "
                    f"{r['minflt']}\n")


def perf_summary(rows):
    """Medians of the perf counters per test and allocator."""
    out = ["\n## perf counters, medians per run of the program\n"]
    groups = {}
    for r in rows:
        if not r.get("perf"):
            continue
        counters = dict(kv.split("=", 1) for kv in r["perf"].split(";") if "=" in kv)
        groups.setdefault((r["test"], r["alloc"]), []).append(counters)
    for (test, alloc), samples in groups.items():
        keys = sorted({k for s in samples for k in s})
        cells = [f"{k}={median([float(s[k]) for s in samples if k in s]):.4g}" for k in keys]
        out.append(f"{test:16} {alloc:8} " + " ".join(cells))
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dir", default="~/mimalloc-bench", help="the mimalloc-bench checkout")
    parser.add_argument("--alloc", action="append", default=[],
                        help="NAME or NAME=PATH of a library to preload; repeat for several")
    parser.add_argument("--tests", default="all", help="comma-separated tests, or all")
    parser.add_argument("--skip", default="", help="comma-separated tests to leave out")
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--profile", choices=["long", "stock"], default="long")
    parser.add_argument("--perf", nargs="?", const=PERF_EVENTS, default=None,
                        help="count these perf events (default: a standard set)")
    parser.add_argument("--perf-cmd", default="sudo -n perf")
    parser.add_argument("--out", help="append every sample to this TSV")
    parser.add_argument("--load", action="append", default=[],
                        help="summarize saved TSV files instead of running")
    parser.add_argument("--baseline", default="mi")
    parser.add_argument("--csv", help="also write the samples as a mimalloc-bench benchres.csv")
    parser.add_argument("--seed", type=int, default=0x5EED)
    parser.add_argument("--all-columns", action="store_true",
                        help="also summarize user and system time and minor faults")
    args = parser.parse_args()

    columns = list(COLUMNS) if args.all_columns else ["metric_s", "maxrss_kib"]
    if args.load:
        rows = [r for path in args.load for r in load(path)]
        print(summarize(rows, args.baseline, columns))
        if any(r.get("perf") for r in rows):
            print(perf_summary(rows))
        if args.csv:
            write_csv(rows, args.csv)
        return

    root = Path(args.dir).expanduser().resolve()
    all_tests = tests(root)
    names = list(all_tests) if args.tests == "all" else args.tests.split(",")
    skip = set(filter(None, args.skip.split(",")))
    names = [n for n in names if n not in skip]
    for n in names:
        if n not in all_tests:
            sys.exit(f"unknown test {n}; known: {', '.join(all_tests)}")
    allocators = resolve_allocators(args.alloc or ["mi"], root)
    rng = random.Random(args.seed)
    out_path = args.out or tempfile.mktemp(prefix="mbrun-", suffix=".tsv")
    new = not os.path.exists(out_path)
    rows = []
    with open(out_path, "a") as out:
        if new:
            out.write("\t".join(TSV_HEADER) + "\n")
        for run in range(args.runs):
            for index, name in enumerate(names):
                test = all_tests[name]
                shift = (run + index) % len(allocators)
                for alloc, lib in allocators[shift:] + allocators[:shift]:
                    sample, error = run_sample(test, root, lib, rng.randrange(4096), args)
                    if sample is None:
                        print(f"run {run}: {name} {alloc}: failed: {error}", file=sys.stderr)
                        continue
                    perf = ";".join(f"{k}={v:.0f}" for k, v in sample.perf.items())
                    row = [name, alloc, str(run), f"{sample.metric:.6f}", f"{sample.wall:.6f}",
                           f"{sample.user:.4f}", f"{sample.sys:.4f}", str(sample.maxrss),
                           str(sample.minflt), str(sample.majflt), perf]
                    out.write("\t".join(row) + "\n")
                    out.flush()
                    rows.append(dict(zip(TSV_HEADER, row)))
                    print(f"run {run}: {name:15} {alloc:8} {sample.metric:9.4f} s "
                          f"{sample.maxrss / 1024:8.1f} MiB", file=sys.stderr, flush=True)
    os.rmdir(SCRATCH)
    print(f"samples in {out_path}", file=sys.stderr)
    print(summarize(rows, args.baseline, columns))
    if args.perf:
        print(perf_summary(rows))
    if args.csv:
        write_csv(rows, args.csv)


if __name__ == "__main__":
    main()
