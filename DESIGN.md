# How bcmalloc works

**Tldr.** bcmalloc is a global allocator for Rust programs that run for a long time, on many threads, and free memory on threads other than the one that allocated it. It reserves one region of address space when the program starts and carves everything out of it. A thread allocates by popping a pointer off a small stack for the size class, and frees by pushing it back, with no atomic instructions and without touching the block. Blocks freed by another thread travel home in messages, so no thread writes into memory that another core holds. Idle memory goes back to the operating system on a clock, in whole huge pages on Linux.

This document explains the design, where each idea comes from, and what each choice costs. Benchmark results close the document.

## 1. Introduction

Most programs never think about their allocator, and that is the right default. A few programs live for months, run hundreds of threads, hand work from one thread to another, and care a great deal about what their memory does under load. A database, a network proxy and a consensus node all look like this. Their allocator needs three things.

1. **A bound.** The program should know up front how much memory it can use, and running out should be a plain refusal and not a slow slide into the OOM killer. TigerBeetle takes this to its conclusion and allocates nothing after start-up[^tigerbeetle]. We want the same discipline while still serving an ordinary Rust program.
2. **No contention on the hot path.** Allocation should touch only memory that belongs to the calling thread. Freeing should be cheap even when the block was allocated somewhere else, because in a pipeline one thread allocates and another frees.
3. **Memory that comes back.** A burst of one size should not strand memory that a later burst of another size needs, and memory that stays idle should go back to the system.

bcmalloc is built from techniques that other allocators have proven, put together around those three goals.

| Idea                                                    | Where it comes from                                |
| ------------------------------------------------------- | -------------------------------------------------- |
| One reservation, sized up front, as a hard limit        | TigerBeetle's static allocation[^tigerbeetle]      |
| Chunks claimed with one atomic change to a bitmap word  | LLFree[^llfree]                                    |
| Any empty slab can become any size class                | jwmalloc's uniform slab pool[^jwmalloc]            |
| Per-thread caches that are arrays of pointers           | jemalloc[^jemalloc] and TCMalloc[^tcmalloc]        |
| Caches that grow when a class keeps missing             | TCMalloc's per-CPU cache sizing[^tcmalloc]         |
| Remote frees sent home to the owning heap in batches    | snmalloc[^snmalloc]                                |
| Blocks return to their owner unless they share no line | Hoard's ownership rule[^hoard]                     |
| Free blocks tracked in a bitmap outside the slab        | jemalloc's slab bitmaps[^jemalloc]                 |
| A map from address to owner and size class              | the pagemaps of TCMalloc and snmalloc              |
| Huge pages filled densely and given back whole          | Temeraire[^temeraire] and mimalloc[^mimalloc-thp]  |
| A realloc that keeps a block that still fits            | mimalloc[^mimalloc]                                |
| One part of the address space per NUMA node             | the node-local arenas common to server allocators  |

## 2. Background

Rust gives a global allocator a small contract[^globalalloc].

- Every free carries the layout that the block was allocated with.
- The allocator must never unwind, and it must not allocate while it allocates. A lot of the standard library allocates, so the allocator stays on the thread-local storage and atomics that are documented not to.
- Returning null means failure, and the caller decides what to do.

The allocator is five layers, and each one is small.

![Threads sit on heaps, heaps keep bins and own slabs, slabs are runs of chunks, chunks live in one bitmap, and the bitmap describes one reservation](assets/layers.svg)

**Figure 1.** The layers. A thread owns a heap. A heap keeps a bin of free block pointers for each size class and owns slabs, each a run of 64 KiB chunks. A bitmap with one bit per chunk says which chunks are in use. Everything lives in one mapping.

## 3. The region

At first use the allocator asks the operating system for one big mapping and never asks again. The mapping is read-write anonymous memory flagged so that the kernel does not charge the whole size against the commit limit. Nothing is backed by a physical page until it is touched, so reserving 64 GiB costs address space and nothing else.

The mapping holds the allocator's own metadata and the memory it hands out, laid out so that the zero-filled pages the kernel provides are already a valid starting state. Nothing is written at start-up.

![A memory map of the reservation with its header, heap slots, bin slots, slab descriptors, chunk map, bitmaps and chunks, and the arithmetic that turns a pointer into its chunk map entry and descriptor](assets/region-map.svg)

**Figure 2.** The reservation. Metadata is about 1% of the region. A pointer finds its chunk map entry with a subtraction and a shift, and its slab descriptor with a mask on top.

Metadata sits next to the memory it describes and never inside it. A buffer overflow in the program cannot corrupt the bookkeeping, and the allocator never writes into a block it has handed out or is about to hand out.

The **chunk map** has one word per chunk. For every chunk of a slab it records the heap that owns the slab and the slab's size class. TCMalloc and snmalloc call this structure a pagemap. Here it is a flat array, because the region is one reservation. A free reads one word of it to learn both the size class and whether the block belongs to the calling thread's heap.

A few parts of the OS interface differ between the supported platforms, and the platform layer hides them.

| Operation    | Linux                              | macOS                          |
| ------------ | ---------------------------------- | ------------------------------ |
| reserve      | mmap with MAP_NORESERVE            | mmap with MAP_NORESERVE        |
| huge pages   | madvise MADV_HUGEPAGE              | none, there is no THP          |
| NUMA parts   | mbind with a preferred node        | none                           |
| populate     | MADV_POPULATE_WRITE, else touch    | touch every page               |
| lock         | mlock                              | mlock                          |
| discard      | MADV_DONTNEED, reads back zeros    | MADV_FREE_REUSABLE, not zeroed |
| reuse        | nothing                            | MADV_FREE_REUSE                |

**Huge pages.** On Linux the region asks for transparent huge pages, as mimalloc does by default there[^mimalloc-thp]. A 2 MiB page needs one TLB entry where 4 KiB pages need 512, so programs whose working set is larger than the TLB covers stop paying for address translation. The cost is that memory is backed, and given back, 2 MiB at a time. Section 9 explains how the allocator keeps that cost down.

**NUMA.** On a machine with more than one NUMA node, the chunks are split into one contiguous part per node, and each part asks the kernel to prefer its node's memory. A heap takes its slabs from the part of the node its thread was running on when it claimed the heap, and borrows from the other parts only when its own is full. A thread's memory therefore sits on its own node without the allocator ever asking where it runs on the fast path.

The configuration decides how eager to be. `Commit::Lazy` touches nothing. `Commit::Populate` backs every page before the first allocation, and `Commit::Lock` also pins them. With those, a program gets TigerBeetle's posture directly. The memory is resident from the start, there are no page faults on the allocation path, and the capacity is a hard limit.

## 4. Chunks

The region is divided into 64 KiB chunks, and a bitmap has one bit per chunk. A set bit means the chunk belongs to a slab or to a huge allocation. This bitmap is the only shared structure on the path to fresh memory.

A request for a run of up to 64 chunks fits in one bitmap word. To find a run of three free chunks the allocator folds the word onto itself, which takes `log2(n)` steps, and every bit that survives is the start of a run. It then flips the run's bits with one compare-and-swap. If another thread changed the word first, the swap fails and returns the new word, and the search runs again on that.

![Folding a bitmap word to find runs of free chunks, then claiming one with a compare-and-swap, and rolling back a claim that spans words](assets/chunks.svg)

**Figure 3.** Claiming chunks. The fold finds every run start in a word with a few shifts and ANDs. The claim is one compare-and-swap. A run longer than a word claims whole words one at a time and gives them back if it loses.

Freeing a run is one `fetch_and` per word it touches. Neighbouring free chunks coalesce simply by being neighbours, because there is nothing to merge. Every transaction is a single atomic change to one word, which is the rule LLFree is built on[^llfree]. There is never a moment when a chunk belongs to nothing, so a concurrent search can never be told that the region is full while memory is free.

Where a run goes matters for memory as much as for speed.

- Single chunks come from the low end of the region, starting at a hint so that threads spread over different words. Longer runs come from the high end. Long-lived small slabs then do not scatter through the address space and break up the room that large runs need.
- A bitmap word covers two 2 MiB huge pages. When a run fits in either half of a word, it goes to the half that is already fuller. Allocations pack into huge pages that are partly used and leave empty ones empty, where they can be given back whole. Temeraire places small allocations by the same principle, by how full each huge page is[^temeraire].
- A run of more than 64 chunks cannot be claimed with one atomic. It claims words in order and rolls back if it loses. That path is obstruction-free and not lock-free, which is acceptable because it only serves allocations over 4 MiB.

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

**Free blocks.** A slab's descriptor holds a bitmap with one bit per block, set while the block is free. Taking a block back sets its bit. A refill collects set bits a word at a time, lowest first, and a word whose bits are all set yields 64 blocks without a search. This is how jemalloc tracks the free regions of its slabs[^jemalloc]. Neither operation reads or writes the memory of a block. A list threaded through the free blocks would make a refill read each block to find the next, every read waiting on the one before, and make every free write into a block whose cache line may be cold or held by another core.

![A slab of blocks and its descriptor, with the free bitmap, the counters and the list links](assets/slab.svg)

**Figure 5.** Anatomy of a slab. The descriptor lives outside the slab and only the owning heap touches it. Blocks in a heap's bins, in messages on their way home, or in the program's hands all count as in use.

A block's index in its slab is its offset divided by the block size. The class sizes are not powers of two, so the division is a multiplication by a precomputed reciprocal and a shift, which is exact for every offset in a slab. jemalloc divides by its bin sizes the same way[^jemalloc-div].

**Reuse across classes.** A slab whose blocks are all free returns its chunks to the bitmap, and any slab can then become any class. The uniform slab pool echoes jwmalloc[^jwmalloc].

![A burst of two block sizes, then the small blocks die and their chunks become slabs of other classes](assets/reuse.svg)

**Figure 6.** Reuse across classes. An allocator that keeps a free list per size class cannot do this, because a freed 64 byte block cannot serve a 4 KiB request. Here the only memory that is ever free for reuse is a whole slab.

Each class keeps one empty slab, so that a loop that allocates and frees one block does not move a slab in and out of the bitmap.

## 6. Heaps and the fast path

A heap is a thread's private view of the region. For each size class it keeps a **bin**, which is a stack of pointers to free blocks, and lists of the slabs it owns that have room and that are full.

![The bins of a heap as stacks of pointers, how a pop and a push work, and how a refill fills a bin from a slab's bitmap](assets/bins.svg)

**Figure 7.** A heap's bins. Allocating pops the top pointer and freeing pushes one. An empty bin is refilled to half from the front slab's bitmap, and a full bin gives its older half back.

The fast path of an allocation is a table lookup for the class and a pop. The fast path of a free is one load from the chunk map and a push. Neither uses an atomic instruction or touches the block. The bins are the thread caches of jemalloc and the per-CPU arrays of TCMalloc[^jemalloc][^tcmalloc]. Because a bin is a stack, a block that was just freed is the next one handed out, while it is still in the cache.

Each bin is a range of pointer slots in the region, with its top pointer kept next to the bin's two limits. Whether the bin is empty or full is a comparison of the low 16 bits of the top pointer with a limit, because a bin spans at most 4 KiB. The thread finds its heap in one thread-local load. On Apple silicon it reads the slot straight from the thread's key table through the thread pointer register, which is the load that the system's own lookup does, without the call. Creating the key stores a marker through the public interface and reads it back through the register, and the key is refused if the two disagree, so a change in the operating system's layout shows up as a failed allocation and not as corruption. Elsewhere the heap pointer sits in an ordinary thread-local.

**Refills and flushes.** An empty bin first looks at the heap's inbox, where other threads send blocks home, and then fills half its capacity from the front slab with room. Blocks come out of each slab in address order, and the bin hands them out in the same order, so a run of allocations walks memory upwards. A slab that runs out moves to the full list. A full bin gives back its older half, and blocks that follow each other in one slab go back with one look at the slab and one write per bitmap word.

**Sizing.** A bin starts at a capacity that holds about 32 KiB of blocks. When a class alternates between running dry and overflowing, which is a sign that its bursts are bigger than the bin, the bin doubles, up to a limit and within a budget of 2 MiB for the whole heap. A bin that has not missed for a while is halved again. This is how TCMalloc sizes its per-CPU caches[^tcmalloc]. A program that builds and drops trees of a few thousand nodes then keeps them all in the bin, and a quiet class holds little.

**Heap lifetime.** A heap is a slot in the region and not a property of a thread. When a thread exits, its heap empties its bins and outboxes and goes back to the pool with all its slabs, and the next thread to start takes it. A thread that is about to go quiet can call `Bcmalloc::flush_thread` to hand back what its heap keeps, as jemalloc's thread cache flush does.

## 7. Freeing from another thread

A free reads the chunk map entry of the block, which names the heap that owns its slab. If that is the calling thread's heap, the block goes into the bin. Otherwise there are two cases.

**Blocks that share no cache line.** If the class size is a multiple of the cache line, 64 bytes on x86-64 and 128 bytes on Apple silicon, and its slab is a single chunk, the block goes into the freeing thread's own bin and can be handed out again there. The block cannot share a line with any other block, because blocks start at multiples of their size. A thread that reuses it therefore never writes next to a block that its owner still uses, which is the false sharing that Hoard's rule of returning blocks to their owner exists to prevent[^hoard]. jemalloc and TCMalloc reuse any freed block in the freeing thread[^jemalloc][^tcmalloc]. bcmalloc keeps that speed where it is safe and limits a held block to pinning one 64 KiB slab.

**Every other block goes home.** It is appended to an outbox for its owner. A heap keeps 16 outboxes, chosen by the owner's id, so up to 16 destinations fill messages side by side. A message is a 512-byte block of the sender's own that holds 62 pointers. When it is full, holds 64 KiB of blocks, or must make room for another destination, the sender pushes it onto the owner's inbox with one compare-and-swap. A message with a single block sends the block itself instead, tagged in its link. snmalloc returns remote frees to their owner the same way, as messages batched per destination[^snmalloc].

![A consumer thread sends blocks to the producer that owns them: the blocks go into an outbox message, the message goes onto the producer's inbox with one compare-and-swap, and the producer files the pointers into its bins](assets/messages.svg)

**Figure 8.** A remote free. The sender writes only pointers, into memory it owns. The owner reads only the message, and files the pointers into its bins without touching the blocks.

The messages carry pointers and are not lists threaded through the blocks. A list would make the sender write into each freed block, whose cache line usually sits in the owner's core, and the owner would then read every block in turn to find the next. With messages, the only memory that moves between cores is the message itself, one line per eight blocks.

The inbox is a stack that any thread pushes onto and that only its owner empties, with one swap. Nothing is ever popped by anyone else, so the stack cannot suffer the ABA problem. The owner reads it when a bin runs dry, which is when it needs blocks, and files each pointer into its bin, or into its slab if the bin is full. The chunk map tells it the class of each block. The message itself is then freed like any other block.

Two properties make this safe. A block in a message still counts as in use in its slab, so the slab cannot be released and its chunk map entry cannot change while the block is on its way. And a heap slot outlives its threads: a heap whose thread has exited keeps its inbox, and the next thread to claim the slot, or a call to `trim`, reads it.

## 8. Reallocation

A small block keeps its place when the new size fits in it and wastes at most half of it, which is the rule mimalloc uses[^mimalloc]. A free takes the class from the chunk map and not from the layout, so a block that a realloc kept is still freed as what it is. A huge block that shrinks gives back the chunks past its new end, and one that grows claims the chunks right after it if they are free. Everything else is a new block and a copy.

## 9. Giving memory back

A program that frees a gigabyte and then runs for a month should not keep the gigabyte. The allocator hands free chunks back to the operating system on a clock, and it needs three small pieces of state to do that safely.

Every bitmap word has a timestamp, which is the latest time that a chunk in it was freed. A second bitmap marks the chunks that were freed since they were last given back. A third marks the chunks whose pages are already gone. The clock runs from paths that are slow anyway, which are refills, slab releases and huge frees. When it is due, and a flag says that some chunk may be waiting, one thread makes a pass over the words. A word whose newest free is at least 10 ms old has its idle chunks claimed with a compare-and-swap on the used word, the same operation that allocation uses, so a chunk in use can never be handed to the kernel. The pass advises the kernel about each claimed run, marks it as given back, and releases it. A program that stays quiet pays nothing, and a busy one pays a clock read now and then.

With huge pages, a pass only takes a 2 MiB page whose 32 chunks are all free. Giving back part of a huge page would split it into small pages and lose the TLB reach it was there for. Temeraire releases whole huge pages for the same reason[^temeraire], and mimalloc's default on Linux is to purge no less than 2 MiB[^mimalloc-thp]. The placement rule of section 4 is what makes whole pages come free: allocations crowd into pages that are already in use.

The next claim of a purged chunk has to know. Allocation checks the given-back bitmap for the run it just claimed and, on macOS, tells the kernel that the pages are in use again. macOS has two ways to give pages back. The lazy one leaves them in the process's footprint until the system is short of memory. The reusable one takes them out of the footprint at once, and wants a matching call before reuse so that the accounting is right. Linux needs no second call, because discarded pages come back as zeros.

A pass makes free chunks look allocated while it holds them. If a pass holds every free chunk, an allocation that looks at the bitmap would fail although memory is free. The allocator therefore does not report that the region is full until it has looked again with no pass overlapping the attempt, and two counters, one for passes running and one for passes finished, tell it whether any did.

`trim` is the explicit version. It empties every heap that no thread holds, then gives back every free chunk, whole huge page or not, because the caller asked for it.

## 10. How it is checked

Concurrency bugs hide from tests that pass, so the allocator is checked in five independent ways.

| Tool                 | What it covers                                                                                        |
| -------------------- | ----------------------------------------------------------------------------------------------------- |
| loom                 | Every interleaving of the chunk bitmap, a purge pass and its flag, heap claims, and inbox pushes       |
| Miri                 | All unit tests under strict provenance, on the portable thread lookup                                  |
| fuzzing              | Random operation sequences against a model, with a validator that checks every structure after a step |
| benchmarks           | Sixteen threads starting at once, and a fresh process for every workload                               |
| independent review   | Every change, read-only, by a second model that writes its own reproductions                           |

The validator deserves a sentence. After every few operations the test harness walks every slab of every heap and checks that each bitmap agrees with its counter, that every slab is on the list its occupancy calls for, and that every pointer in a bin, an outbox, a message or an inbox is a block of the right class that is not free in its slab and appears nowhere else. A bin may hold another heap's block only if its class allows reuse. Several heaps are driven from one thread, which makes cross-heap frees deterministic, so a failing fuzz input replays as a unit test. The real global allocator is also exercised end to end. The integration test binary runs the standard test harness on top of bcmalloc, with producer and consumer threads and a few thousand short-lived threads.

Memory ordering is the other thing to state clearly. A chunk claim is an `Acquire` compare-and-swap, and a free is a `Release` `fetch_and`. A purge pass claims with `AcqRel`, so a thread that finds a chunk missing because of a pass also sees the counters of that pass. An inbox push is a `Release` compare-and-swap and the owner's swap is an `Acquire`, which carries the sender's writes, the message and everything the program wrote into the blocks, to the owner. The chunk map is written when a slab starts and read with relaxed loads, because every thread that frees a block got it through some synchronisation of its own that carries the write along.

## 11. Where it wins, and what it costs

The design spends a little memory and some bookkeeping to keep three things cheap: the common allocation and free, memory that moves between threads, and memory that has to come back.

**The common path does almost nothing.** An allocation is a table lookup and a pop, and a free is one load from a dense, read-mostly map and a push. Neither uses an atomic instruction, and neither reads or writes the block. A freed block is the next one handed out, so a program that allocates and frees in a loop works in a few cache lines.

**Memory that moves between threads does not drag cache lines with it.** A block freed on another core is never written by the freeing thread and never walked by the owner. Only the messages travel, one line for every eight blocks. Blocks of classes that cannot share a line are reused where they are freed, which is what makes a thread pool that passes buffers around about as fast as one that does not.

**Scattered frees stay cheap.** A free that lands in a cold part of memory costs a bitmap bit and not a write into the block, and a refill hands out a slab's free blocks in address order whatever order they came back in. Programs that free large structures in random order, and then build new ones, get memory that is laid out for them again.

**Memory comes back.** Any empty slab can serve any class, idle chunks go back to the system on a clock, and on Linux they go back in whole huge pages that the placement rule keeps whole. The capacity is a hard limit, and with `Commit::Lock` the allocator never takes a page fault at all.

**Locality is kept without asking.** Huge pages widen the reach of the TLB, and on a machine with several NUMA nodes a heap takes its memory from its own node. Neither costs anything on the fast path.

These are the costs.

- **Metadata.** The free bitmaps make a slab descriptor 640 bytes, so metadata is about 1% of the region instead of a fraction of that. It is address space until a slab uses it.
- **Bursts larger than a bin.** A block that overflows a bin goes back to its slab and comes out again on the next refill, one bitmap operation each way, where a list would move a whole chain at once. Bins that keep overflowing grow to make this rare, within a budget.
- **Blocks that share lines always go home.** For small classes the freeing thread cannot keep the block, so a producer and a consumer pay one message per batch, and blocks wait in an outbox until it fills, is needed for another destination, or is flushed.
- **Per-thread caches hold memory.** A heap's bins and outboxes keep blocks that other threads cannot use until the heap flushes them, and a thread that goes quiet keeps them until it exits or calls `flush_thread`.
- **Huge pages raise the floor.** Memory is backed and returned 2 MiB at a time, so a lightly used region holds more than it would with small pages.

## 12. Limits

- Alignments above 64 KiB are refused.
- A thread that finds every heap taken fails its allocations, which aborts the program. `max_heaps` is a design-time bound, like TigerBeetle's connection limit.
- Allocations over 4 MiB claim several bitmap words and are obstruction-free, not lock-free.
- The capacity is a hard limit. A purge pass or a trim makes some chunks look allocated while it holds them, so a request that needs those chunks waits for the pass to finish. It does not fail.
- A heap holds blocks in its bins and outboxes until it needs them, fills a message, or is flushed. A thread that goes quiet keeps them until it exits or calls `flush_thread`. The bins of one heap hold at most their initial capacity plus 2 MiB.
- Memory in a slab that is only partly free is never returned, and with huge pages neither is a huge page that is partly in use, except by `trim`.
- Fresh pages are zero, but the allocator does not track that, so zeroed allocations always write zeros.
- The NUMA parts are fixed when the region is created, and a heap keeps the node of the CPU its thread first ran on.

## 13. Results

The benchmark suite has 43 workloads in eight groups, including producer and consumer rings, Larson, false sharing, standard collections, resizing and large blocks, plus two that measure memory. Every workload runs in a fresh process, three times per allocator with the allocators taking turns in a rotating order, and the best run counts. `just bench` reproduces everything.

![Bars for eight kinds of workload showing each allocator's speed relative to mimalloc on macOS and on Linux, and a chart of the memory still held after a burst is freed](assets/benchmarks.svg)

**Figure 9.** Speed relative to mimalloc, as the geometric mean of each group, on both machines, and the memory still held after a burst is freed.

On an Apple M4 Max with macOS 26.5, where the system allocator is libmalloc:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 0.51   | 3.17     | 1.00     | 0.50     | 1.55     |
| speed, bulk            | 0.28   | 1.35     | 1.00     | 0.40     | 0.73     |
| speed, churn           | 0.25   | 2.36     | 1.00     | 1.36     | 1.59     |
| speed, sharing         | 0.56   | 2.94     | 1.00     | 2.12     | 1.56     |
| speed, false sharing   | 0.47   | 0.99     | 1.00     | 0.71     | 1.00     |
| speed, collections     | 0.67   | 1.22     | 1.00     | 0.65     | 1.12     |
| speed, resizing        | 0.89   | 1.04     | 1.00     | 0.99     | 0.88     |
| speed, large           | 0.99   | 1.16     | 1.00     | 0.15     | 0.12     |
| speed, all workloads   | 0.49   | 1.95     | 1.00     | 0.82     | 1.15     |
| speed, mean of groups  | 0.52   | 1.60     | 1.00     | 0.67     | 0.88     |
| memory held afterwards | 3.46   | 7.60     | 1.00     | 0.98     | 6.88     |
| peak resident memory   | 1.05   | 1.00     | 1.00     | 0.99     | 1.11     |

On an AMD EPYC 9354P with 32 cores and Linux 7.0 with transparent huge pages, where the system allocator is glibc 2.39:

| Compared with mimalloc | system | bcmalloc | mimalloc | jemalloc | snmalloc |
| ---------------------- | ------ | -------- | -------- | -------- | -------- |
| speed, pair            | 1.07   | 2.72     | 1.00     | 0.66     | 1.68     |
| speed, bulk            | 0.12   | 1.13     | 1.00     | 0.33     | 0.55     |
| speed, churn           | 0.80   | 2.18     | 1.00     | 1.77     | 1.83     |
| speed, sharing         | 0.32   | 2.21     | 1.00     | 2.17     | 1.08     |
| speed, false sharing   | 0.99   | 1.00     | 1.00     | 0.98     | 1.00     |
| speed, collections     | 0.58   | 1.04     | 1.00     | 0.71     | 0.95     |
| speed, resizing        | 1.02   | 1.10     | 1.00     | 0.60     | 0.90     |
| speed, large           | 0.44   | 1.08     | 1.00     | 0.08     | 0.23     |
| speed, all workloads   | 0.61   | 1.71     | 1.00     | 0.86     | 1.12     |
| speed, mean of groups  | 0.55   | 1.44     | 1.00     | 0.64     | 0.88     |
| memory held afterwards | 33.70  | 7.48     | 1.00     | 0.98     | 1.01     |
| peak resident memory   | 2.20   | 1.08     | 1.00     | 1.85     | 1.50     |

Above 1.00 is better than mimalloc, for speed and for memory alike. The all-workloads row counts every workload once, and the mean-of-groups row counts every group once. On macOS bcmalloc is the fastest allocator on 37 of the 43 workloads and within 5% of the fastest on four more. It trails jemalloc when one vector grows to millions of elements and on the channel with four producers. On Linux it is the fastest on 25 and within 5% on seven more. jemalloc is ahead on the channels, where the consumer frees blocks it never reuses. glibc is ahead on single very large blocks and on growing a large vector, and snmalloc by under 15% on the smallest blocks of a single thread and on the tree rewrite. After a 256 MiB burst is freed, bcmalloc holds under 5 MiB on both machines, where mimalloc and jemalloc hold about 260.

[^tigerbeetle]: [A Database Without Dynamic Memory Allocation](https://tigerbeetle.com/blog/2022-10-12-a-database-without-dynamic-memory/) explains why TigerBeetle sizes everything from fixed limits at start-up and what that buys.
[^llfree]: [LLFree, scalable and optionally-persistent page-frame allocation](https://www.usenix.org/conference/atc23/presentation/wrenger) is a lock-free frame allocator whose every transaction is a single atomic change of a single cache line.
[^jwmalloc]: [jwmalloc, a verified memory allocator for mobile devices](https://www.usenix.org/conference/osdi26/presentation/wang-jiawei) uses a uniform slab size with pooling so that an emptied slab is reusable by any size class.
[^jemalloc]: The [jemalloc manual](https://jemalloc.net/jemalloc.3.html) describes its thread caches, which hold freed objects of each size class for reuse by the thread, and its slabs, whose free regions are tracked in a bitmap.
[^jemalloc-div]: jemalloc computes region indices with [a reciprocal multiplication instead of a division](https://github.com/jemalloc/jemalloc/blob/dev/src/div.c).
[^tcmalloc]: The [TCMalloc design](https://github.com/google/tcmalloc/blob/master/docs/design.md) describes its per-CPU caches, which are arrays of object pointers, the transfer cache between them, and how a cache's capacity grows when it alternates between running dry and overflowing.
[^snmalloc]: [snmalloc, a message passing allocator](https://www.microsoft.com/en-us/research/wp-content/uploads/2020/04/snmalloc.pdf) returns remote frees to the owning allocator as batched messages.
[^hoard]: [Hoard, a scalable memory allocator for multithreaded applications](https://people.cs.umass.edu/~emery/pubs/berger-asplos2000.pdf) shows how allocators cause false sharing between threads, and avoids it by returning freed blocks to the heap that owns their superblock.
[^temeraire]: [Beyond malloc efficiency to fleet efficiency](https://www.usenix.org/conference/osdi21/presentation/hunter) describes Temeraire, the hugepage-aware back end of TCMalloc, which fills huge pages densely so that whole ones come free.
[^mimalloc]: [mimalloc, free list sharding in action](https://www.microsoft.com/en-us/research/publication/mimalloc-free-list-sharding-in-action/) describes the per-thread heaps, the per-page free lists and the cross-thread free list.
[^mimalloc-thp]: The [mimalloc README](https://github.com/microsoft/mimalloc) documents that it uses transparent huge pages on Linux by default and then purges no less than 2 MiB at a time.
[^globalalloc]: The contract is in the documentation for [GlobalAlloc](https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html), including the rules on unwinding and re-entrance.
