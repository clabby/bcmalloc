# How bcmalloc works

**Tldr.** bcmalloc is a global allocator for Rust programs that run for a long time, on many threads, and free memory on threads other than the one that allocated it. It reserves one region of address space when the program starts and carves everything out of it. A small allocation pops a block off a thread-local list with no atomic instructions, and a free from another thread is one compare-and-swap. On an Apple M4 Max it allocates small blocks faster than mimalloc and jemalloc (144 against 121 and 131 million operations per second), and it uses less memory than mimalloc on our mixed benchmark (62 against 90 MiB). It is two to three times slower than the best allocator on blocks of 512 B to 2 KiB, and that is the first thing we would tune. This post walks through the design, the tests that keep it honest, and the numbers.

## 1. Introduction

Most programs never think about their allocator, and that is the right default. A few programs live for months, run hundreds of threads, hand work from one thread to another, and care a great deal about what their memory does under load. A database, a network proxy and a consensus node all look like this. Their allocator needs three things.

1. **A bound.** The program should know up front how much memory it can use, and running out should be a plain refusal and not a slow slide into the OOM killer. TigerBeetle takes this to its conclusion and allocates nothing after start-up, which designs away fragmentation, out-of-memory surprises and use-after-free[^tigerbeetle]. We want the same discipline while still serving an ordinary Rust program.
2. **No contention on the hot path.** Allocation should touch only memory that belongs to the calling thread. Freeing should be cheap even when the block was allocated somewhere else, because in a pipeline one thread allocates and another frees.
3. **Memory that comes back.** A burst of one size should not strand memory that a later burst of another size needs.

bcmalloc is built around those three. It is not a drop-in replacement for malloc in every program, it does not support Windows, and it has no idea about NUMA. It supports Linux and macOS.

The design borrows from several places. The per-thread heaps and per-page free lists come from mimalloc[^mimalloc]. Batched, message-style returns of remote frees come from snmalloc[^snmalloc]. The idea that one atomic update on one cache line should be the whole transaction comes from LLFree[^llfree], and so does the habit of keeping allocator metadata dense and addressable by arithmetic. The uniform slab pool that lets any empty slab become any size class echoes jwmalloc[^jwmalloc].

## 2. Background

Rust gives a global allocator a small contract[^globalalloc].

- Every free carries the layout that the block was allocated with, so the allocator knows the size class without reading a header.
- The allocator must never unwind, and it must not allocate while it allocates. A lot of the standard library allocates, so the allocator stays on the thread-local storage and atomics that are documented not to.
- Returning null means failure, and the caller decides what to do.

The first point shapes everything. Because a free knows its size, the allocator keeps no per-block header and no size table. A pointer and a layout are enough to find the slab, and the slab is enough to find the owner.

The whole thing has five layers, and each one is small.

![Threads sit on heaps, heaps own slabs, slabs are runs of chunks, chunks live in one bitmap, and the bitmap describes one reservation](assets/layers.svg)

**Figure 1.** The layers. A thread owns a heap, a heap owns slabs grouped by size class, and a slab is a run of 64 KiB chunks. A bitmap with one bit per chunk says which chunks are in use. Everything lives in one mapping.

## 3. The region

At first use the allocator asks the operating system for one big mapping and never asks again. The mapping is read-write anonymous memory flagged so that the kernel does not charge the whole size against the commit limit. Nothing is backed by a physical page until it is touched, so reserving 64 GiB costs address space and nothing else.

The mapping holds the allocator's own metadata and the memory it hands out, laid out so that the zero-filled pages the kernel provides are already a valid starting state. Nothing is written at start-up.

![A memory map of the reservation with its header, heap slots, slab descriptors, bitmap and chunks, and the arithmetic that turns a pointer into a descriptor](assets/region-map.svg)

**Figure 2.** The reservation. The metadata is about 0.2% of the region. A pointer finds its slab descriptor with a subtraction, a shift and a mask.

We did not start here. The first prototype was a `static` array, which is the obvious way to get a fixed heap. It works for small sizes and then falls over. A zeroed static of 6 GiB did not finish compiling in the several minutes we gave it, because the compiler evaluates the array. Reserving at run time with `mmap` has no such limit, and a test in the crate allocates 5 GiB from an 8 GiB region.

Two parts of the OS interface differ between the supported platforms, and the platform layer hides both.

| Operation | Linux                           | macOS                       |
| --------- | ------------------------------- | --------------------------- |
| reserve   | mmap with MAP_NORESERVE         | mmap with MAP_NORESERVE     |
| huge page | madvise MADV_HUGEPAGE           | none, there is no THP       |
| populate  | MADV_POPULATE_WRITE, else touch | touch every page            |
| lock      | mlock                           | mlock                       |
| discard   | MADV_DONTNEED, reads back zeros | MADV_FREE, lazy, not zeroed |

The configuration decides how eager to be. `Commit::Lazy` touches nothing. `Commit::Populate` backs every page before the first allocation, and `Commit::Lock` also pins them. With those, a program can ask for TigerBeetle's posture directly. The memory is resident from the start, there are no page faults on the allocation path, and the capacity is a hard limit.

## 4. Chunks

The region is divided into 64 KiB chunks, and a bitmap has one bit per chunk. A set bit means the chunk belongs to a slab or to a huge allocation. This bitmap is the only shared structure on the path to fresh memory, so it has to be good.

A request for a run of up to 64 chunks fits in one bitmap word. To find a run of three free chunks the allocator folds the word onto itself, which takes `log2(n)` steps, and every bit that survives is the start of a run. It then flips the run's bits with one compare-and-swap. If another thread changed the word first, the swap fails and returns the new word, and the search runs again on that.

![Folding a bitmap word to find runs of free chunks, then claiming one with a compare-and-swap, and rolling back a claim that spans words](assets/chunks.svg)

**Figure 3.** Claiming chunks. The fold finds every run start in a word with a few shifts and ANDs. The claim is one compare-and-swap. A run longer than a word claims whole words one at a time and gives them back if it loses.

Freeing a run is one `fetch_and` per word it touches. Neighbouring free chunks coalesce simply by being neighbours, because there is nothing to merge.

Two details matter in practice. Single chunks come from the low end of the region and longer runs come from the high end, which keeps long-lived small slabs from breaking up the room that large runs need. And a run of more than 64 chunks cannot be claimed with one atomic, so it claims words in order and rolls back if it loses. That path is obstruction-free and not lock-free, which is acceptable because it only serves allocations over 4 MiB.

We considered a buddy allocator first, with a bitmap per level, because splitting and merging are what people expect from a page allocator. It does not survive contact with concurrency. Splitting a block passes through a moment where the memory belongs to no bitmap, and an allocation that scans at that moment sees nothing free. On a nearly empty heap the very first allocation splits the only block, so another thread starting at the same time could be told the heap is full. A flat bitmap has no such moment, because the single atomic update is the whole transaction. LLFree states the same rule outright[^llfree-atomic].

## 5. Size classes and slabs

A request of up to 512 KiB is rounded up to one of 104 size classes. Classes are 16 bytes apart up to 128 bytes. Above that there are eight per doubling, so rounding wastes at most one part in eight.

![Rounding waste across the size classes, with the three slab sizes shaded](assets/size-classes.svg)

**Figure 4.** Rounding waste by request size. Each dot is the worst request for one class. The three shaded bands show which slab size serves which blocks.

Each class is carved out of slabs, which are runs of chunks aligned to their own size. Bigger blocks get bigger slabs, so every slab holds at least eight blocks and the unusable tail is smaller than one block.

| Block size        | Slab        | Blocks per slab |
| ----------------- | ----------- | --------------- |
| 16 B to 8 KiB     | 64 KiB (1)  | 8 to 4096       |
| 8 KiB to 64 KiB   | 512 KiB (8) | 8 to 56         |
| 64 KiB to 512 KiB | 4 MiB (64)  | 8 to 56         |

Alignment falls out of the geometry. Every class size is a multiple of 16 and every slab starts on a multiple of its own size, so every block is 16-byte aligned. A request for more alignment is rounded up to a power of two at least that large, and a block of such a class starts at a multiple of its size. The largest alignment supported is one chunk, 64 KiB. Anything above 512 KiB gets a run of whole chunks to itself.

The slab is also what makes memory come back. A slab whose blocks are all free returns its chunks to the bitmap, and any slab can then become any class.

![A burst of two block sizes, then the small blocks die and their chunks become slabs of other classes](assets/reuse.svg)

**Figure 5.** Reuse across classes. An allocator that keeps a free list per size class cannot do this, because a freed 64 byte block cannot serve a 4 KiB request. Here the only memory that is ever free for reuse is a whole slab.

## 6. Heaps and the fast path

A heap is a thread's private view of the region. For each size class it keeps a list of slabs that have room and a list of slabs that are full. Allocating pops the free list of the front slab. That is the entire fast path, and it uses no atomics.

A slab has three lists and a frontier.

![A row of blocks in a slab with the free, local_free and remote lists threaded through them, and the descriptor split across two cache lines](assets/slab.svg)

**Figure 6.** Anatomy of a slab. The blocks that have never been used are not on any list. The descriptor lives outside the slab and keeps the owner's fields on one cache line and the shared field on another.

The three lists exist for a reason. `free` is the list the allocator pops from. `local_free` collects blocks the owning thread freed, and it is spliced into `free` only when `free` is empty, so the fast path never looks at it. `remote` collects blocks freed by other threads. A slab hands out fresh blocks lazily, a few at a time from the bump frontier, so a new 4 MiB slab touches only the memory it uses.

When a slab runs dry the heap folds in the other lists, then extends the frontier, and only then parks the slab on the full list and moves on. When a class has no slab with room it adopts slabs that other threads have freed into, or takes a fresh one from the bitmap. A slab whose last block is freed goes back to the bitmap, except that each class keeps one so a block allocated and freed in a loop does not bounce a slab in and out.

A heap is a slot in the region and not a property of a thread. When a thread exits, its heap goes back to the pool with all its slabs, and the next thread to start takes it. Memory held by a departed thread never has to be handed over.

## 7. Freeing from another thread

This is the part that took the most care.

When a block is freed, the allocator finds its slab by arithmetic and compares the slab's owner with the freeing heap. If they match, the block goes on the local list with a plain store. If they differ, the block is pushed onto the slab's `remote` list with one compare-and-swap. The owner empties that list in a single swap the next time it needs blocks, and because only the owner ever removes anything, the list cannot suffer the ABA problem.

That is enough for slabs the owner is still looking at. A full slab is the hard case. The owner has stopped looking at it, so a block freed into it would sit there until something happened to check. The owner needs to hear about the first remote free, exactly once.

The `remote` word carries the whole protocol. It holds the head of the list in its high bits and two flags in the low bits, which are free because blocks are 16-byte aligned. The owner marks a slab full by swapping the word from empty to `FULL`. A remote free pushes its block and, in the same compare-and-swap, turns `FULL` into `QUEUED`. The thread that made that change, and only that one, pushes the slab on the owner's stack of slabs to revisit.

![The owner marks a slab full, another thread frees into it and notifies the owner, further frees stay silent, and the owner drains the stack and takes the blocks](assets/remote-free.svg)

**Figure 7.** A remote free into a full slab. Each side learns what it needs from the result of its own compare-and-swap, and nobody looks at the descriptor a second time.

The properties that fall out of this are worth saying plainly. The owner can only mark a slab full when no block is pending, because the swap expects an empty word. A pusher learns atomically whether it must notify, so a notification is never sent twice and never lost. And nobody reads the descriptor after the swap that could have let the owner recycle it. The last block of a slab may be freed remotely, so that matters. A slab with a pending notification cannot be recycled, because the notification still points at it, and the sticky `QUEUED` flag records that.

We did not trust the argument. The remote word has loom models for the races that matter. Marking full races a remote free. Clearing the mark races a remote free. Two pushers race for the notification. A full heap-level model frees two blocks into a full slab from another thread while the owner allocates, and then checks that both blocks come back with no flush. To check that the models can fail, we broke the code on purpose. Replacing the compare-and-swap in the mark with a load and a store made loom report "marked full but nobody will notify the owner". Deleting the call that queues the slab made the heap model report that a freed block went missing.

## 8. How it is checked

Concurrency bugs hide from tests that pass, so there are four independent layers and each has found something.

| Tool    | What it covers                                                              | What it found                                        |
| ------- | --------------------------------------------------------------------------- | ---------------------------------------------------- |
| loom    | Every interleaving of the chunk bitmap, the slab word, and the heap handoff | Nothing new, but it caught every deliberate mutation |
| Miri    | All unit tests, with strict provenance                                      | A misplaced cfg that only broke the Miri build       |
| fuzzing | Random operation sequences against a model, across heaps                    | A single-chunk request that ignored its alignment    |
| benches | Sixteen threads starting at once                                            | A heap claim that failed with free slots             |

The heap claim bug shows why one layer is not enough. Loom did not find it, because nothing modelled two threads claiming at once until the benchmark crashed. The first version limited its search to the slots that had been claimed so far plus one, so that thread start-up would not touch every slot. Two threads starting together both wanted the same next slot, and the loser was told no slot was free. It aborted the benchmark with a failed 64 KiB allocation. The loom model for it is two threads and two slots, and it fails on the old code every time. The fix is to scan from slot zero and stop at the first free slot, which touches at most one unused slot anyway.

Every test uses the same helpers. A shadow model records each live block, checks alignment, bounds and overlap, and fills every block with a stamp that is verified when it is freed or reallocated. The same model drives the unit tests, the random-traffic test, and the fuzz target, so a failing fuzz input replays as a unit test. Several heaps are driven from one thread, which makes cross-heap frees deterministic. The real global allocator is also exercised end to end. The integration test binary runs the whole standard test harness on top of bcmalloc, with producer and consumer threads and a few thousand short-lived threads.

Memory ordering is the other thing to state clearly. A chunk claim is an `Acquire` compare-and-swap, and a free is a `Release` `fetch_and`. A block is usually written by one thread, freed, and reused by another, and the release on the free and the acquire on the claim carry the first thread's writes to the second. The remote list uses the same pairing. A push is a `Release` compare-and-swap and the owner's take is an `Acquire`.

## 9. Results

All numbers come from an Apple M4 Max with 12 performance and 4 efficiency cores, macOS 26.5 and Rust 1.100. Each is the best of five runs.

![Grouped bars for nine workloads, one bar per allocator, scaled to the fastest, and a peak memory chart](assets/benchmarks.svg)

**Figure 8.** The benchmark suite against the system allocator, mimalloc, jemalloc and snmalloc.

The honest summary is that bcmalloc is competitive and not the fastest. It is ahead of macOS's system allocator everywhere except vec growth, it beats mimalloc on cross-thread frees and memory, and it is within a factor of 2.3 of the leader on every throughput row. It loses most on thread scaling of the mixed churn, where jemalloc pulls ahead at 8 and 16 threads, and on cross-thread frees, where jemalloc is about twice as fast.

The mixed churn number hides the more interesting result. We ran the same loop through every allocator with only one kind of block at a time.

| Blocks         | bcmalloc | mimalloc | jemalloc  | snmalloc  |
| -------------- | -------- | -------- | --------- | --------- |
| 16 B to 128 B  | 143.7    | 121.3    | 130.9     | **211.2** |
| 512 B to 2 KiB | 46.3     | 59.9     | **112.1** | 76.4      |
| 4 KiB          | 79.2     | 22.7     | **122.8** | 23.0      |

Millions of allocate-and-free pairs per second on one thread. Small blocks are where the design works as intended. The fast path is a few loads and a store, and it is faster than mimalloc and jemalloc. Mid-sized blocks are where it hurts. A slab of 2 KiB blocks holds 32 of them, and a loop that frees random blocks leaves every slab sparsely free. The front slab runs dry after a few allocations, so the slow path runs constantly. Each run of the slow path moves a slab between lists and does a compare-and-swap to mark it full, and each free into a full slab undoes that. jemalloc avoids this with bins and thread caches that decouple the allocation path from slab state. We have not done that yet, and it is the first thing we would tune.

Two other rows deserve an explanation. Peak memory is low, which we attribute to freed slabs returning to a pool that any class can use, though we have not isolated the cause. The large-block row is about 0.2 million operations per second for bcmalloc, mimalloc and the system allocator, and about 25 times lower for jemalloc and snmalloc. Our guess, which we have not verified, is that those two give pages back to the OS on every free, so the benchmark takes a page fault on every page it touches. bcmalloc only returns pages when asked, with `Region::trim`, which is the behaviour a program that reserves up front wants.

## 10. Limits and what comes next

bcmalloc is a working allocator and not a finished one. These are the limits we know about.

- Alignments above 64 KiB are refused.
- A thread that finds every heap taken fails its allocations, which aborts the program. `max_heaps` is a design-time bound, like TigerBeetle's connection limit.
- Allocations over 4 MiB claim several bitmap words and are obstruction-free, not lock-free.
- The capacity is a hard limit, and a trim pass makes some chunks look allocated to other threads while it runs.
- A slab abandoned by an exited thread stays with its heap until a new thread claims that heap or `trim` sweeps it.
- Fresh pages are zero, but the allocator does not track that, so `alloc_zeroed` always writes zeros.

Next we would tune the slab transitions, add a small per-thread cache in front of them, and batch remote frees by destination slab as BatchIt does[^batchit]. A per-CPU backend through restartable sequences would bound the heap count by the number of CPUs and not the number of threads. Giving the region a NUMA node, tracking zeroed pages, and handing back huge pages in 2 MiB units like Temeraire[^temeraire] all fit in the same structure.

How fast is fast enough? For the programs this is meant for, the answer is mostly in the tail and in the memory curve and not in the single-thread number. The structure here keeps both predictable. The bound is explicit, the hot path touches only the caller's memory, and every cross-thread interaction is one compare-and-swap on one word that a model checker has seen every interleaving of.

[^tigerbeetle]: [A Database Without Dynamic Memory Allocation](https://tigerbeetle.com/blog/2022-10-12-a-database-without-dynamic-memory/) explains why TigerBeetle sizes everything from fixed limits at start-up and what that buys.
[^mimalloc]: [mimalloc, a new, high-performance, scalable memory allocator for the modern era](https://www.microsoft.com/en-us/research/blog/mimalloc-a-high-performance-scalable-memory-allocator-for-the-modern-era/) describes the per-thread heaps, the three free lists per page and the cross-thread free list.
[^snmalloc]: [snmalloc, a message passing allocator](https://www.microsoft.com/en-us/research/wp-content/uploads/2020/04/snmalloc.pdf) returns remote frees to the owning allocator in batches with one atomic exchange per batch.
[^llfree]: [LLFree, scalable and optionally-persistent page-frame allocation](https://www.usenix.org/conference/atc23/presentation/wrenger) is a lock-free frame allocator whose transactions are single atomic changes to one cache line.
[^llfree-atomic]: The LLFree paper says that every transaction authoritatively happens within a single atomic change of a single cache line. The chunk bitmap follows that rule, and our reason for it (no moment where memory belongs to nothing) is our own.
[^jwmalloc]: [jwmalloc, a verified memory allocator for mobile devices](https://www.usenix.org/conference/osdi26/presentation/wang-jiawei) uses a uniform slab size with pooling so that an emptied slab is reusable by any size class.
[^globalalloc]: The contract is in the documentation for [GlobalAlloc](https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html), including the rules on unwinding and re-entrance.
[^batchit]: [BatchIt](https://www.microsoft.com/en-us/research/publication/batchit-optimizing-message-passing-allocators-for-producer-consumer-workloads-an-intellectual-abstract/) caches remote frees per slab so that frees to the same slab share one atomic operation.
[^temeraire]: [Temeraire](https://www.usenix.org/conference/osdi21/presentation/hunter) is the hugepage-aware allocator in TCMalloc.
