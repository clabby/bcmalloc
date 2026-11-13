# How bcmalloc works

**Tldr.** bcmalloc is a global allocator for Rust programs that run for a long time, on many threads, and free memory on threads other than the one that allocated it. It reserves one region of address space when the program starts and carves everything out of it. A small allocation pops a block off a thread-local list with no atomic instructions and without touching the slab it came from. On an Apple M4 Max it is the fastest of five allocators on 32 of 43 workloads, and 1.64 times faster than mimalloc by geometric mean (1.43 times when each of eight kinds of workload counts once). jemalloc is still faster when threads free each other's blocks and when one huge vector grows. bcmalloc returns idle memory to the operating system on a timer, so after a 256 MiB burst is freed it holds 2.8 MiB where mimalloc holds 261. This post walks through the design, the tests that keep it honest, the numbers, and the directions we think are worth taking next.

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

**Figure 1.** The layers. A thread owns a heap, a heap owns slabs grouped by size class, and a slab is a run of 64 KiB chunks. Bitmaps with one bit per chunk say which chunks are in use and which are idle. Everything lives in one mapping.

## 3. The region

At first use the allocator asks the operating system for one big mapping and never asks again. The mapping is read-write anonymous memory flagged so that the kernel does not charge the whole size against the commit limit. Nothing is backed by a physical page until it is touched, so reserving 64 GiB costs address space and nothing else.

The mapping holds the allocator's own metadata and the memory it hands out, laid out so that the zero-filled pages the kernel provides are already a valid starting state. Nothing is written at start-up.

![A memory map of the reservation with its header, heap slots, slab descriptors, bitmap and chunks, and the arithmetic that turns a pointer into a descriptor](assets/region-map.svg)

**Figure 2.** The reservation. The metadata is about 0.4% of the region. A pointer finds its slab descriptor with a subtraction, a shift and a mask.

We did not start here. The first prototype was a `static` array, which is the obvious way to get a fixed heap. It works for small sizes and then falls over. A zeroed static of 6 GiB did not finish compiling in the several minutes we gave it, because the compiler evaluates the array. Reserving at run time with `mmap` has no such limit, and a test in the crate allocates 5 GiB from an 8 GiB region.

A few parts of the OS interface differ between the supported platforms, and the platform layer hides them.

| Operation | Linux                           | macOS                          |
| --------- | ------------------------------- | ------------------------------ |
| reserve   | mmap with MAP_NORESERVE         | mmap with MAP_NORESERVE        |
| huge page | madvise MADV_HUGEPAGE           | none, there is no THP          |
| populate  | MADV_POPULATE_WRITE, else touch | touch every page               |
| lock      | mlock                           | mlock                          |
| discard   | MADV_DONTNEED, reads back zeros | MADV_FREE_REUSABLE, not zeroed |
| reuse     | nothing                         | MADV_FREE_REUSE                |

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

A heap is a thread's private view of the region. For each size class it keeps a hot list of blocks, a list of slabs that have room and a list of slabs that are full. Allocating pops the hot list. That is the entire fast path. It uses no atomics and it does not read the slab the block belongs to.

Getting to that pop takes a handful of instructions and we counted them. A request of up to 8 KiB with the usual alignment goes through a table indexed by its size in 16 byte steps, which gives the size class in one load. The thread's heap comes from a slot in the thread's key table. On Apple silicon the allocator reads that slot directly through the thread pointer register, which is the load that the system's own routine does, without the call. The standard thread-local costs a call into the dynamic loader there, and removing it roughly doubled the speed of the smallest blocks. When the allocator creates its key it stores a marker through the public interface and reads it back through the register, and it refuses the key if the two disagree, so a change in the operating system's layout shows up as a failed allocation and not as corruption. Everywhere else the heap sits in an ordinary thread-local, and a key destructor gives the heap back when the thread exits.

A slab has three lists and a frontier.

![A row of blocks in a slab with the free, local_free and remote lists threaded through them, and the descriptor split across two cache lines](assets/slab.svg)

**Figure 6.** Anatomy of a slab. The blocks that have never been used are not on any list. The descriptor lives outside the slab and keeps the owner's fields on one line and the shared field on another, and each line is 128 bytes.

The three lists exist for a reason. `free` holds the blocks for the next refill. `local_free` collects blocks the owning thread freed, and it is spliced into `free` only when `free` is empty. `remote` collects blocks freed by other threads. A slab hands out fresh blocks lazily, a few at a time from the bump frontier, so a new 4 MiB slab touches only the memory it uses. The slab also keeps the length of each list, so it never has to walk one to know how many blocks it holds.

The hot list is what makes the fast path short. When it runs dry the heap folds in the other lists of the front slab, or extends the frontier, and then takes the slab's whole free list at once and makes it the hot list. From then on the slab counts those blocks as in use, which is true from its point of view, so handing them out one at a time costs the slab nothing. A slab is never parked, retired or handed back to the bitmap while its blocks are in a hot list, because the heap only moves a slab when its hot list is empty, and a flush returns the blocks first.

When a slab runs dry the heap parks it on the full list and moves on. When a class has no slab with room it adopts slabs that other threads have freed into, or takes a fresh one from the bitmap. A slab whose last block is freed goes back to the bitmap, except that each class keeps one so a block allocated and freed in a loop does not bounce a slab in and out.

Where a slab goes when a block comes back matters more than it looks. The first version put a full slab that got one block back at the front of the active list. The next allocation took that block, the slab was full again, and almost every operation moved a slab between lists. Now such a slab goes to the back. Allocation keeps draining the front slab, and by the time the returning slab reaches the front it has collected many blocks. That one change took single-thread churn on mixed sizes from 63 to 125 million operations per second.

The other change with a surprising effect was padding. The descriptor was split into an owner half and a shared half on separate 64 byte lines, which is the textbook layout. The M4 has 128 byte lines, so both halves shared one, and every remote free took the line away from the owner. Aligning the halves to 128 bytes, and giving the heap slot the same treatment so that the unfull stack sits apart from the owner's lists, raised eight-thread Larson from 594 to 808 million operations per second with no other change. The crate pads to 128 bytes on aarch64 and x86-64, where adjacent lines are fetched together, and to 64 bytes elsewhere.

A heap is a slot in the region and not a property of a thread. When a thread exits, its heap goes back to the pool with all its slabs and its hot lists, and the next thread to start takes it. Memory held by a departed thread never has to be handed over.

## 7. Freeing from another thread

This is the part that took the most care.

When a block is freed, the allocator finds its slab by arithmetic and compares the slab's owner with the freeing heap. If they match, the block goes on the local list with a plain store. If they differ, the block is pushed onto the slab's `remote` list with one compare-and-swap. The owner empties that list in a single swap the next time it needs blocks, and because only the owner ever removes anything, the list cannot suffer the ABA problem.

That is enough for slabs the owner is still looking at. A full slab is the hard case. The owner has stopped looking at it, so a block freed into it would sit there until something happened to check. The owner needs to hear about the first remote free, exactly once.

The `remote` word carries the whole protocol. It holds the head of the list in its high bits and two flags in the low bits, which are free because blocks are 16-byte aligned. The owner marks a slab full by swapping the word from empty to `FULL`. A remote free pushes its block and, in the same compare-and-swap, turns `FULL` into `QUEUED`. The thread that made that change, and only that one, pushes the slab on the owner's stack of slabs to revisit.

![The owner marks a slab full, another thread frees into it and notifies the owner, further frees stay silent, and the owner drains the stack and takes the blocks](assets/remote-free.svg)

**Figure 7.** A remote free into a full slab. Each side learns what it needs from the result of its own compare-and-swap, and nobody looks at the descriptor a second time.

The properties that fall out of this are worth saying plainly. The owner can only mark a slab full when no block is pending, because the swap expects an empty word. A pusher learns atomically whether it must notify, so a notification is never sent twice and never lost. And nobody reads the descriptor after the swap that could have let the owner recycle it. The last block of a slab may be freed remotely, so that matters. A slab with a pending notification cannot be recycled, because the notification still points at it, and the sticky `QUEUED` flag records that.

We did not trust the argument. The remote word has loom models for the races that matter. Marking full races a remote free. Clearing the mark races a remote free. Two pushers race for the notification. A full heap-level model frees two blocks into a full slab from another thread while the owner allocates, and then checks that both blocks come back with no flush. To check that the models can fail, we broke the code on purpose. Replacing the compare-and-swap in the mark with a load and a store made loom report "marked full but nobody will notify the owner". Deleting the call that queues the slab made the heap model report that a freed block went missing.

The remote word is not touched once per free. A heap keeps, for each size class, a batch of the blocks it has freed into one slab of another heap. They are chained through their first words, and the chain is pushed with one compare-and-swap when the batch holds 16 KiB of blocks or 64 blocks, when a block of another slab of that class arrives, or when the heap is flushed or released. We added this after measuring a ring of threads that pass blocks to each other. Each remote free stored a link into a block that another core owned, and then a release compare-and-swap had to wait for that store, so every free paid a full cross-core miss. With a batch the stores are in flight together and one operation publishes them. The price is that blocks sit in a batch, and the owner cannot reuse them, until the batch is handed over. That is at most 16 KiB per class per heap, and a heap hands everything over when it is released.

## 8. Giving memory back

A program that frees a gigabyte and then runs for a month should not keep the gigabyte. The allocator hands free chunks back to the operating system on a clock, and it needs three small pieces of state to do that safely.

Every bitmap word has a timestamp, which is the latest time that a chunk in it was freed. A second bitmap marks the chunks that were freed since they were last given back. A third marks the chunks whose pages are already gone. The clock runs from paths that are slow anyway, which are a slab refill, a slab release and a huge free. When it is due, and a flag says that some chunk may be waiting, one thread makes a pass over the words. A word whose newest free is at least 10 ms old has its idle chunks claimed with a compare-and-swap on the used word, the same operation that allocation uses, so a chunk in use can never be handed to the kernel. The pass advises the kernel about each claimed run, marks it as given back, and releases it. A program that stays quiet pays nothing, and a busy one pays a clock read now and then.

The next claim of a purged chunk has to know. Allocation checks the given-back bitmap for the run it just claimed and, on macOS, tells the kernel that the pages are in use again. That matters because macOS has two ways to give pages back. The lazy one leaves them in the process's footprint until the system is short of memory. The reusable one takes them out of the footprint at once, and wants a matching call before reuse so that the accounting is right. Linux needs no second call, because discarded pages come back as zeros.

A pass makes free chunks look allocated while it holds them, and that has a cost that tests found. If a pass holds every free chunk, an allocation that looks at the bitmap fails although memory is free. The allocator therefore does not report that the region is full until it has looked again with no pass overlapping the attempt, and two counters, one for passes running and one for passes finished, tell it whether any did. The retry looks at the bitmap on every round, so a pass that was descheduled cannot stop an allocation that other free chunks could serve.

The result is the memory row of the benchmarks. After 256 MiB is allocated in 1 KiB blocks and every block is freed, bcmalloc holds 2.8 MiB 800 ms later. mimalloc holds 261 and jemalloc 222, and only snmalloc, at 5.5, is close.

## 9. How it is checked

Concurrency bugs hide from tests that pass, so there are five independent layers and each has found something.

| Tool               | What it covers                                                                                      | What it found                                                                               |
| ------------------ | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| loom               | Every interleaving of the chunk bitmap, the slab word with chain pushes, a purge pass, and its flag | A purge pass that made an allocation fail with memory free, and a flag that stranded memory |
| Miri               | All unit tests, with strict provenance, on the portable thread lookup                               | A cached table pointer that kept the provenance of one descriptor                           |
| fuzzing            | Random operation sequences against a model, with a validator that walks every list after each step  | A single-chunk request that ignored its alignment                                           |
| benches            | Sixteen threads starting at once, and a fresh process for every workload                            | A heap claim that failed with free slots, and a fourfold slowdown of huge frees             |
| independent review | Every revision, read-only, by a second model that writes its own reproductions                      | Two leaked heap claims, a retry that waited on a stalled pass, a race in the validator      |

The validator deserves a sentence. After every few operations the test harness walks every slab list of every heap and checks that each counter equals the real length of its list, that no block appears twice anywhere, that every hot list belongs to its front slab, that the flags in each remote word agree with the list the slab is on, and that every batch holds blocks of one foreign slab. A deliberate mutation that forgot to reset a counter was caught at once, by an arithmetic overflow and by the validator. The validator reads only what the claimant may read. Review found a version that read another heap's private fields, which is a data race the moment two threads run, and it now works from the geometry of the region instead.

The heap claim bug shows why one layer is not enough. Loom did not find it, because nothing modelled two threads claiming at once until the benchmark crashed. The first version limited its search to the slots that had been claimed so far plus one, so that thread start-up would not touch every slot. Two threads starting together both wanted the same next slot, and the loser was told no slot was free. It aborted the benchmark with a failed 64 KiB allocation. The loom model for it is two threads and two slots, and it fails on the old code every time. The fix is to scan from slot zero and stop at the first free slot, which touches at most one unused slot anyway.

Every test uses the same helpers. A shadow model records each live block, checks alignment, bounds and overlap, and fills every block with a stamp that is verified when it is freed or reallocated. The same model drives the unit tests, the random-traffic test, and the fuzz target, so a failing fuzz input replays as a unit test. Several heaps are driven from one thread, which makes cross-heap frees deterministic. The real global allocator is also exercised end to end. The integration test binary runs the whole standard test harness on top of bcmalloc, with producer and consumer threads and a few thousand short-lived threads.

Memory ordering is the other thing to state clearly. A chunk claim is an `Acquire` compare-and-swap, and a free is a `Release` `fetch_and`. A purge pass claims with `AcqRel`, so a thread that finds a chunk missing because of a pass also sees the counters of that pass. A block is usually written by one thread, freed, and reused by another, and the release on the free and the acquire on the claim carry the first thread's writes to the second. The remote list uses the same pairing. A push is a `Release` compare-and-swap and the owner's take is an `Acquire`.

## 10. Results

All numbers come from an Apple M4 Max with 12 performance and 4 efficiency cores, macOS 26.5 and Rust 1.100. The suite has 43 workloads in eight groups, plus two that measure memory. Every workload runs in a fresh process, three times per allocator with the allocators taking turns in a rotating order, and the tables keep the best run. The system allocator is macOS libmalloc. ~just bench~ reproduces everything.

![Bars for eight kinds of workload showing each allocator's speed relative to mimalloc, and a chart of the memory still held after a burst is freed](assets/benchmarks.svg)

**Figure 8.** Speed relative to mimalloc, as the geometric mean of each group, and the memory still held after a burst is freed.

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.47   | 2.18     | 1.00     | 0.65     | 0.73     |
| speed, bulk            | 0.25   | 1.45     | 1.00     | 0.39     | 0.62     |
| speed, churn           | 0.23   | 2.25     | 1.00     | 1.28     | 1.32     |
| speed, sharing         | 0.54   | 1.91     | 1.00     | 2.00     | 1.43     |
| speed, false sharing   | 0.49   | 1.00     | 1.00     | 1.00     | 1.00     |
| speed, collections     | 0.61   | 1.21     | 1.00     | 0.66     | 1.09     |
| speed, resizing        | 0.98   | 0.92     | 1.00     | 0.99     | 0.88     |
| speed, large           | 0.94   | 1.16     | 1.00     | 0.15     | 0.14     |
| speed, all workloads   | 0.46   | 1.64     | 1.00     | 0.86     | 0.92     |
| speed, mean of groups  | 0.50   | 1.43     | 1.00     | 0.71     | 0.76     |
| memory held afterwards | 3.45   | 9.59     | 1.00     | 1.07     | 6.88     |
| peak resident memory   | 1.12   | 1.10     | 1.00     | 1.03     | 1.14     |

Above 1.00 is better than mimalloc, for speed and for memory alike. The all-workloads row counts every workload once, so a group with many workloads weighs more, and the mean-of-groups row counts every group once. The honest summary is that bcmalloc is the fastest allocator on 32 of the 43 workloads and within 5% of the fastest on three more. The two false-sharing workloads are a tie, because every modern allocator keeps threads off each other's cache lines, and they are there to show that nothing here splits one. The system allocator is about twice as slow on them.

The wins are widest where the fast path and the cache lines matter.

| Workload (millions of operations per second) | bcmalloc | mimalloc | jemalloc | snmalloc |
| -------------------------------------------- | -------- | -------- | -------- | -------- |
| allocate and free 16 B, one thread           | 415      | 233      | 151      | 361      |
| allocate and free 1 KiB, one thread          | 290      | 171      | 144      | 224      |
| mixed sizes with a live set, 1 thread        | 201      | 84       | 105      | 111      |
| mixed sizes with a live set, 16 threads      | 2128     | 741      | 1269     | 844      |
| Larson, 8 threads                            | 1233     | 699      | 793      | 509      |
| binary trees                                 | 184      | 138      | 61       | 152      |

The losses are in the remaining places, and there are eight of them.

| Workload (millions of operations per second) | bcmalloc | Fastest       | Gap   |
| -------------------------------------------- | -------- | ------------- | ----- |
| ring of 8 threads passing batches            | 285      | jemalloc 593  | 2.1x  |
| growing a vector of a million numbers        | 661      | jemalloc 1377 | 2.1x  |
| channel with 4 producers and 4 consumers     | 104      | jemalloc 168  | 1.6x  |
| realloc to random sizes                      | 8.6      | mimalloc 12.6 | 1.5x  |
| shared table swapped by 8 threads            | 70       | jemalloc 118  | 1.7x  |
| channel with 1 producer and 1 consumer       | 47       | jemalloc 63   | 1.3x  |
| ring of 4 threads passing batches            | 308      | jemalloc 403  | 1.3x  |
| hash map with replaced entries               | 14.3     | snmalloc 15.2 | 1.06x |

Four of the losses are one story. In the ring, the channel and the shared table, threads free blocks that other threads allocated. jemalloc keeps such a block in the freeing thread's cache and reuses it there, so nothing ever travels back. bcmalloc sends every block home, now in batches, which cut the gap on the 8-thread ring from about four times to two, and it does not close. That explanation comes from how jemalloc is built and not from a profile of this run. Vector growth is the other large gap. bcmalloc allocates a new run and copies at every doubling. We believe jemalloc and the system allocator extend the block in place or move its pages, and we have not checked. We have not profiled the realloc workload at all.

Three further rows are worth reading closely. Peak memory is about 10% below mimalloc over the whole suite. The two memory workloads are the ones where no allocator is ahead of the pack except by returning idle memory, and bcmalloc now does. The half-freed workload, where a random half of 128 MiB of small blocks is freed, shows 153 MiB for every allocator, because none of them can return memory from a slab that is only partly free. Finally the large-block row, where jemalloc and snmalloc are twenty times slower, is the same effect as before, and our guess is unchanged. They give pages back on every free and take a page fault on every page the benchmark touches. bcmalloc waits ten milliseconds, so a loop that frees and allocates the same big block never pays.

The changes that bought the most are worth listing, because each was found by measuring and each looks obvious afterwards. The workload is named as the suite stood when we made the change, so the figures come from separate runs.

| Change                                    | Workload                  | Before | After |
| ----------------------------------------- | ------------------------- | ------ | ----- |
| Requeue a returning slab at the back      | mixed churn, one thread   | 63     | 125   |
| Pad descriptors and heap slots to 128 B   | Larson, 8 threads         | 594    | 808   |
| Hot lists, size table, direct thread slot | allocate and free 16 B    | 195    | 410   |
| Hand over remote frees in batches         | ring of 4 threads         | 229    | 308   |
| Return idle chunks on a clock             | memory held after a burst | 261    | 2.8   |

The last row is in MiB and the others are millions of operations per second.

## 11. Limits and what comes next

bcmalloc is a working allocator and not a finished one. These are the limits we know about.

- Alignments above 64 KiB are refused.
- A thread that finds every heap taken fails its allocations, which aborts the program. `max_heaps` is a design-time bound, like TigerBeetle's connection limit.
- Allocations over 4 MiB claim several bitmap words and are obstruction-free, not lock-free.
- The capacity is a hard limit. A purge pass or a trim makes some chunks look allocated while it holds them, so a request that needs those chunks waits for the pass to finish. It does not fail.
- A heap holds back up to 16 KiB of blocks per class that it freed into other heaps' slabs, until the batch fills, moves on or is flushed. A thread that goes quiet leaves them there until it exits.
- Memory in a slab that is only partly free is never returned. No allocator in the suite does this, and the half-freed workload shows it.
- Fresh pages are zero, but the allocator does not track that, so zeroed allocations always write zeros.
- A slab abandoned by an exited thread stays with its heap until a new thread claims that heap or `trim` sweeps it.
- Nothing has run on Linux yet. The portable thread lookup that Linux uses is tested on macOS by forcing it with a build flag, and the Linux half of the platform layer has only unit tests that this machine could not run. The direct slot read exists for aarch64 macOS only.
- Every number comes from one machine.

These are the directions we think are worth taking, in the order we would try them. Most have a measurement behind them, and the last two are ideas that we have not yet measured.

1. **A cache of foreign blocks.** The ring, the channel and the shared table are all the same gap, 1.3 to 2.1 times behind jemalloc. A small bounded stack per class of blocks that belong to other heaps' slabs would let the freeing thread reuse them for its own allocations and spill the rest through the batch path that exists. The slab owner stays the same, so the remote protocol does not change. The costs are that those slabs stay pinned while their blocks are held and that the bound needs a policy. It needs loom models for the owner learning about a slab whose blocks are cached elsewhere, and a fuzz operation that frees across heaps with the cache full.
2. **Moving pages instead of copying them for huge reallocations.** Growing a vector is 2.1 times behind. Linux has `mremap` and macOS has `mach_vm_remap`, either of which moves a run's pages to a new address in the region without a copy. Growing in place when the next chunks are free is cheaper still, but huge runs are placed from the high end of the region, which leaves no room after them, so that would change placement too. The Linux half cannot be checked on the machine we have.
3. **The realloc path and the path above 8 KiB.** Random reallocation is 1.5 times behind mimalloc and we have not profiled it. Sizes above the table take a function call and the arithmetic classification, and the hash map workload is 6% behind snmalloc. A profile should say whether a second table, an inlined medium path or a cheaper same-class test is the right fix.
4. **A per-CPU backend through restartable sequences on Linux.** It would bound the heap count by the number of CPUs and not the number of threads, which also removes the heap-exhaustion limit.
5. **Tuning the purge clock.** The interval is 10 ms and the timestamp covers a whole bitmap word of 64 chunks. A finer stamp, claiming recently purged runs last so that they stay purged, and purging sooner when the region is nearly full are all cheap to try. On Linux the lazy discard may beat the eager one on the refault cost, and that needs measuring there.
6. **Latency and not only throughput.** The suite measures operations per second. The tails, which are the point for a database or a consensus node, come from a refill, a frontier extension, a purge pass and the first allocation, and none of them is measured. The Apple counter ticks at 24 MHz, so per-operation timing needs batches.
7. **Memory from partly used slabs.** Compaction in the style of Mesh[^mesh] could return memory that nothing here can, at the price of changing what a block's address means.
8. **Zero tracking, NUMA and huge pages.** Remembering which chunks are still zero would let zeroed allocations from fresh memory skip the write. A region per NUMA node, and handing back huge pages in 2 MiB units like Temeraire[^temeraire], fit in the same structure.
9. **Validation on Linux and x86-64.** Run the suite and the fuzz targets there, and add the direct slot read for x86-64 macOS if anyone needs it.

How fast is fast enough? For the programs this is meant for, the answer is mostly in the tail and in the memory curve and not in the single-thread number. The structure here keeps both predictable. The bound is explicit, the hot path touches only the caller's memory, idle memory goes back on a clock, and every cross-thread interaction is one compare-and-swap on one word that a model checker has seen every interleaving of.

[^tigerbeetle]: [A Database Without Dynamic Memory Allocation](https://tigerbeetle.com/blog/2022-10-12-a-database-without-dynamic-memory/) explains why TigerBeetle sizes everything from fixed limits at start-up and what that buys.
[^mimalloc]: [mimalloc, a new, high-performance, scalable memory allocator for the modern era](https://www.microsoft.com/en-us/research/blog/mimalloc-a-high-performance-scalable-memory-allocator-for-the-modern-era/) describes the per-thread heaps, the three free lists per page and the cross-thread free list.
[^snmalloc]: [snmalloc, a message passing allocator](https://www.microsoft.com/en-us/research/wp-content/uploads/2020/04/snmalloc.pdf) returns remote frees to the owning allocator in batches with one atomic exchange per batch.
[^llfree]: [LLFree, scalable and optionally-persistent page-frame allocation](https://www.usenix.org/conference/atc23/presentation/wrenger) is a lock-free frame allocator whose transactions are single atomic changes to one cache line.
[^llfree-atomic]: The LLFree paper says that every transaction authoritatively happens within a single atomic change of a single cache line. The chunk bitmap follows that rule, and our reason for it (no moment where memory belongs to nothing) is our own.
[^jwmalloc]: [jwmalloc, a verified memory allocator for mobile devices](https://www.usenix.org/conference/osdi26/presentation/wang-jiawei) uses a uniform slab size with pooling so that an emptied slab is reusable by any size class.
[^globalalloc]: The contract is in the documentation for [GlobalAlloc](https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html), including the rules on unwinding and re-entrance.
[^batchit]: [BatchIt](https://www.microsoft.com/en-us/research/publication/batchit-optimizing-message-passing-allocators-for-producer-consumer-workloads-an-intellectual-abstract/) caches remote frees per slab so that frees to the same slab share one atomic operation.
[^temeraire]: [Temeraire](https://www.usenix.org/conference/osdi21/presentation/hunter) is the hugepage-aware allocator in TCMalloc.
[^mesh]: [Mesh](https://www.usenix.org/conference/pldi19/presentation/powers) compacts memory by merging pages whose live blocks do not overlap and remapping them.
