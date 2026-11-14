//! Runs real programs on top of `Bcmalloc`. This needs its own test binary because a program has
//! one global allocator, and everything the test harness allocates goes through it too.

#![cfg(not(miri))]

use bcmalloc::{Bcmalloc, Commit, Config};
use std::{
    collections::{BTreeMap, HashMap},
    sync::mpsc,
    thread,
};

#[global_allocator]
static GLOBAL: Bcmalloc =
    Bcmalloc::with_config(Config::new().capacity(16 << 30).max_heaps(256).commit(Commit::Lazy));

/// A deterministic filler so that corrupted or mixed-up blocks are noticed.
fn pattern(seed: usize, len: usize) -> Vec<u8> {
    (0..len).map(|i| (seed.wrapping_mul(31) ^ i.wrapping_mul(7)) as u8).collect()
}

#[test]
fn standard_collections_keep_their_contents() {
    let mut strings: HashMap<usize, String> = HashMap::new();
    let mut tree = BTreeMap::new();
    for i in 0..20_000 {
        strings.insert(i, format!("value-{i}-{}", "x".repeat(i % 50)));
        tree.insert(i.wrapping_mul(2_654_435_761) % 100_003, pattern(i, i % 300 + 1));
    }
    for i in (0..20_000).step_by(3) {
        strings.remove(&i);
    }
    for (i, string) in &strings {
        assert!(string.starts_with(&format!("value-{i}-")));
    }
    for (key, bytes) in &tree {
        assert!(!bytes.is_empty(), "entry {key} lost its contents");
    }
}

#[test]
fn growing_vectors_survive_every_reallocation() {
    let mut numbers = Vec::new();
    for i in 0..2_000_000_u64 {
        numbers.push(i);
    }
    assert!(numbers.iter().enumerate().all(|(i, &n)| n == i as u64));
    numbers.shrink_to_fit();
    assert_eq!(numbers.len(), 2_000_000);
}

#[test]
fn over_aligned_types_are_aligned() {
    #[repr(align(64))]
    struct Line([u8; 64]);
    #[repr(align(4096))]
    struct Page([u8; 4096]);
    #[repr(align(65536))]
    struct Chunk([u8; 100]);

    let lines: Vec<Box<Line>> = (0..100).map(|_| Box::new(Line([1; 64]))).collect();
    assert!(lines.iter().all(|b| b.0[0] == 1 && std::ptr::from_ref(&**b).addr() % 64 == 0));
    let pages: Vec<Box<Page>> = (0..20).map(|_| Box::new(Page([2; 4096]))).collect();
    assert!(pages.iter().all(|b| b.0[0] == 2 && std::ptr::from_ref(&**b).addr() % 4096 == 0));
    let chunk = Box::new(Chunk([3; 100]));
    assert_eq!(std::ptr::from_ref(&*chunk).addr() % 65536, 0);
    assert_eq!(chunk.0[99], 3);
    assert_eq!(0u128.to_ne_bytes().len(), 16);
}

#[test]
fn large_allocations_are_zeroed_and_independent() {
    let mut big = vec![0u8; 200 << 20];
    assert!(big.iter().step_by(4096).all(|&b| b == 0));
    big[0] = 1;
    *big.last_mut().unwrap() = 2;
    let other = vec![0u8; 50 << 20];
    assert!(other.iter().step_by(4096).all(|&b| b == 0));
    assert_eq!((big[0], big[big.len() - 1]), (1, 2));
}

#[test]
fn blocks_freed_on_other_threads_are_reused() {
    const PRODUCERS: usize = 4;
    const MESSAGES: usize = 20_000;
    let (sender, receiver) = mpsc::sync_channel::<(usize, Vec<u8>)>(256);
    let consumer = thread::spawn(move || {
        let mut received = 0;
        for (seed, bytes) in receiver {
            assert_eq!(bytes, pattern(seed, bytes.len()), "block {seed} was corrupted in flight");
            received += 1;
        }
        received
    });
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let sender = sender.clone();
            thread::spawn(move || {
                for i in 0..MESSAGES {
                    let seed = p * MESSAGES + i;
                    sender.send((seed, pattern(seed, 16 + seed % 400))).unwrap();
                }
            })
        })
        .collect();
    drop(sender);
    producers.into_iter().for_each(|p| p.join().unwrap());
    assert_eq!(consumer.join().unwrap(), PRODUCERS * MESSAGES);
}

#[test]
fn short_lived_threads_recycle_heaps() {
    // Far more threads than heaps over the program's life, never more than a few at once.
    for _ in 0..40 {
        let batch: Vec<_> = (0..16)
            .map(|t| {
                thread::spawn(move || {
                    let data: Vec<Vec<u8>> = (0..200).map(|i| pattern(t, 1 + i)).collect();
                    data.iter().map(Vec::len).sum::<usize>()
                })
            })
            .collect();
        for handle in batch {
            assert_eq!(handle.join().unwrap(), (1..=200).sum::<usize>());
        }
    }
    let stats = GLOBAL.stats().unwrap();
    assert!(stats.heaps_in_use < 100, "heaps were not recycled: {stats:?}");
}

#[test]
fn trimming_reports_consistent_statistics() {
    let before = GLOBAL.stats().unwrap();
    assert_eq!(before.capacity, 16 << 30);
    drop(vec![0u8; 64 << 20]);
    GLOBAL.trim();
    let after = GLOBAL.stats().unwrap();
    assert!(after.free <= after.capacity);
}

#[cfg(feature = "allocator-api")]
#[test]
fn the_allocator_api_can_be_used_next_to_the_global_allocator() {
    let mut text = String::new();
    let mut numbers = Vec::new_in(&GLOBAL);
    for i in 0..10_000_u32 {
        numbers.push(i);
        if i % 1000 == 0 {
            text.push_str(&i.to_string());
        }
    }
    assert_eq!(numbers.iter().map(|&n| u64::from(n)).sum::<u64>(), 49_995_000);
    assert_eq!(text, "0100020003000400050006000700080009000");
    let boxed = Box::new_in([1_u16; 1000], &GLOBAL);
    assert_eq!(boxed.iter().map(|&n| u32::from(n)).sum::<u32>(), 1000);
}
