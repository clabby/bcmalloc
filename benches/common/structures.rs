//! Workloads built from ordinary Rust collections and trees, where the allocator is a means
//! to an end.

use std::{
    collections::{BTreeMap, HashMap, LinkedList},
    hint::black_box,
};

use super::harness::{Outcome, Rng, Scale, timed};

/// A hash map that keeps replacing random entries with new ones of other sizes.
pub(crate) fn hash_map(scale: Scale) -> Outcome {
    let steps = scale.ops(2_000_000);
    timed(|| {
        let mut rng = Rng::new(21);
        let mut map: HashMap<u64, Vec<u8>> = HashMap::new();
        for key in 0..100_000_u64 {
            map.insert(key, vec![1; rng.between(8, 96)]);
        }
        for step in 0..steps as u64 {
            map.remove(&(rng.below(100_000 + step as usize) as u64));
            map.insert(100_000 + step, vec![1; rng.between(8, 96)]);
        }
        black_box(&map);
        steps as u64
    })
}

/// A B-tree whose values are small boxes that come and go.
pub(crate) fn btree_map(scale: Scale) -> Outcome {
    let steps = scale.ops(1_000_000);
    timed(|| {
        let mut rng = Rng::new(22);
        let mut map: BTreeMap<u64, Box<[u64; 4]>> = BTreeMap::new();
        for key in 0..100_000_u64 {
            map.insert(key, Box::new([key; 4]));
        }
        for step in 0..steps as u64 {
            map.remove(&(rng.below(100_000 + step as usize) as u64));
            map.insert(100_000 + step, Box::new([step; 4]));
        }
        black_box(&map);
        steps as u64
    })
}

/// Formats a few hundred thousand strings, sorts them and glues them together.
pub(crate) fn strings(scale: Scale) -> Outcome {
    let rounds = scale.ops(12);
    timed(|| {
        let mut count = 0;
        for round in 0..rounds {
            let mut rng = Rng::new(round as u64 + 30);
            let mut items: Vec<String> =
                (0..200_000).map(|i| format!("item-{}-{}", rng.next() % 100_000, i)).collect();
            items.sort_unstable();
            let joined = items.join(",");
            count += items.len() as u64;
            black_box(joined);
        }
        count
    })
}

/// A long list of boxed nodes that is built, walked and dropped.
pub(crate) fn linked_list(scale: Scale) -> Outcome {
    let rounds = scale.ops(8);
    timed(|| {
        for _ in 0..rounds {
            let mut list = LinkedList::new();
            for i in 0..1_000_000_u64 {
                list.push_back(i);
            }
            black_box(list.iter().sum::<u64>());
        }
        rounds as u64 * 1_000_000
    })
}

struct Node {
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

fn build(height: u32) -> Option<Box<Node>> {
    (height > 0).then(|| Box::new(Node { left: build(height - 1), right: build(height - 1) }))
}

fn count(node: &Option<Box<Node>>) -> u64 {
    node.as_ref().map_or(0, |node| 1 + count(&node.left) + count(&node.right))
}

/// The classic allocation benchmark. It builds and drops full binary trees, which is a flood of
/// tiny blocks that all die together.
pub(crate) fn binary_trees(scale: Scale) -> Outcome {
    let reps = scale.ops(24);
    timed(|| {
        let long_lived = build(16);
        let mut nodes = 0;
        for _ in 0..reps {
            nodes += count(&build(16));
        }
        black_box(&long_lived);
        nodes
    })
}

/// A big tree in which random subtrees are replaced by new ones, so that old and new nodes end up
/// mixed together in memory and the allocator's locality decides how fast the tree can be walked.
pub(crate) fn tree_rewrite(scale: Scale) -> Outcome {
    const HEIGHT: u32 = 17;
    let rewrites = scale.ops(40_000);
    timed(|| {
        let mut rng = Rng::new(40);
        let mut root = build(HEIGHT);
        let mut nodes = 0;
        for _ in 0..rewrites {
            let depth = rng.between(6, 12) as u32;
            let mut slot = &mut root;
            for _ in 0..depth {
                let Some(node) = slot else { break };
                slot = if rng.next() & 1 == 0 { &mut node.left } else { &mut node.right };
            }
            *slot = build(HEIGHT - depth);
            nodes += (1_u64 << (HEIGHT - depth)) - 1;
        }
        black_box(count(&root));
        nodes
    })
}
struct ValueNode {
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
    value: u64,
}

fn build_values(height: u32, next: &mut u64) -> Option<Box<ValueNode>> {
    (height > 0).then(|| {
        *next += 1;
        let value = *next;
        Box::new(ValueNode {
            value,
            left: build_values(height - 1, next),
            right: build_values(height - 1, next),
        })
    })
}

fn sum(node: &Option<Box<ValueNode>>) -> u64 {
    node.as_ref()
        .map_or(0, |node| node.value.wrapping_add(sum(&node.left)).wrapping_add(sum(&node.right)))
}

/// Binary trees again, but each node holds a value, and the program walks every tree four times and
/// sums the values before it drops the tree. The walks run over the memory that the allocator laid
/// out, so a layout with good locality pays off, and the allocator is only part of the time. One
/// operation is a node.
pub(crate) fn binary_trees_with_work(scale: Scale) -> Outcome {
    const HEIGHT: u32 = 16;
    const WALKS: u32 = 4;
    let reps = scale.ops(300);
    timed(|| {
        let long_lived = build_values(HEIGHT, &mut 0);
        let mut nodes = 0;
        let mut total = 0_u64;
        for _ in 0..reps {
            let mut built = 0;
            let tree = build_values(HEIGHT, &mut built);
            for _ in 0..WALKS {
                total = total.wrapping_add(sum(black_box(&tree)));
            }
            nodes += built;
        }
        black_box((&long_lived, total));
        nodes
    })
}
