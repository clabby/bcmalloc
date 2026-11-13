//! Workloads that grow collections one element at a time, which is a long chain of reallocations.

use std::hint::black_box;

use super::harness::{Outcome, Scale, timed};

/// Pushes a million numbers onto an empty vector.
pub(crate) fn vec_growth(scale: Scale) -> Outcome {
    const ELEMENTS: u64 = 1_000_000;
    let reps = scale.ops(40);
    timed(|| {
        for _ in 0..reps {
            let mut numbers = Vec::new();
            for i in 0..ELEMENTS {
                numbers.push(i);
            }
            black_box(&numbers);
        }
        reps as u64 * ELEMENTS
    })
}

/// Appends characters to a string until it is a few megabytes long.
pub(crate) fn string_growth(scale: Scale) -> Outcome {
    const BYTES: u64 = 4_000_000;
    let reps = scale.ops(40);
    timed(|| {
        for _ in 0..reps {
            let mut text = String::new();
            for _ in 0..BYTES {
                text.push('x');
            }
            black_box(&text);
        }
        reps as u64 * BYTES
    })
}

/// Thousands of vectors that grow in turn, so every size class is resizing at once and the
/// blocks being replaced are spread out.
pub(crate) fn many_vectors(scale: Scale) -> Outcome {
    let rounds = scale.ops(8);
    timed(|| {
        let mut pushes = 0;
        for _ in 0..rounds {
            let mut vectors: Vec<Vec<u32>> = (0..2048).map(|_| Vec::new()).collect();
            for i in 0..2000_u32 {
                for vector in &mut vectors {
                    vector.push(i);
                }
            }
            pushes += 2048 * 2000;
            black_box(&vectors);
        }
        pushes
    })
}
