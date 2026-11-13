//! Deterministic input generation shared by the workloads.

/// A tiny xorshift generator, so that every allocator sees the same inputs.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value in 0..n.
    pub(crate) const fn below(&mut self, n: usize) -> usize {
        ((self.next() >> 8) % n as u64) as usize
    }

    /// A value in lo..=hi.
    pub(crate) const fn between(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// An element of a non-empty slice.
    pub(crate) const fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Words for names, prose and identifiers.
pub(crate) const WORDS: &[&str] = &[
    "alpha", "bravo", "cargo", "delta", "echo", "forge", "gamma", "harbor", "index", "junction",
    "kernel", "lantern", "matrix", "nebula", "orbit", "packet", "quartz", "relay", "socket",
    "tensor", "union", "vector", "window", "xenon", "zenith", "buffer", "cursor", "driver",
    "engine", "filter", "gateway", "handler", "iterator", "journal", "ledger", "mapper", "notice",
    "option", "parser", "queue", "reader", "stream", "thread", "uplink", "vendor", "writer",
    "anchor", "bridge", "circuit", "domain", "entry", "fabric", "grid", "hash", "input", "jitter",
    "key", "layer", "module", "node", "output", "pool", "quota", "route", "state", "token",
];

/// Appends `count` random words separated by single spaces.
pub(crate) fn push_words(out: &mut String, rng: &mut Rng, count: usize) {
    for i in 0..count {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(rng.pick(WORDS));
    }
}

/// A word with its first letter in upper case.
pub(crate) fn capitalized(rng: &mut Rng) -> String {
    let word = rng.pick(WORDS);
    let mut chars = word.chars();
    chars.next().map(|first| first.to_ascii_uppercase()).into_iter().chain(chars).collect()
}
