//! Deterministic random-number generation.
//!
//! Every generation decision flows through [`Rng`] so that a
//! (seed, options, clojure-smith version) triple always produces
//! byte-identical output. The RNG deliberately uses only the most stable
//! `rand` APIs (`ChaCha8Rng::seed_from_u64` and `Rng::next_u64`) plus
//! hand-rolled combinators, so routine dependency updates cannot silently
//! change the random stream.
//!
//! Note: identical output across clojure-smith *versions* is not promised;
//! the PRNG algorithm is pinned per release via `Cargo.lock`.

use rand::Rng as _;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// A seeded, deterministic random number generator.
#[derive(Debug, Clone)]
pub struct Rng {
    inner: ChaCha8Rng,
}

impl Rng {
    /// Create a new RNG from a 64-bit seed.
    pub fn new(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
        }
    }

    /// Create an independent child RNG derived from this one. Drawing from
    /// the child does not disturb this sequence beyond the single draw used
    /// to seed the child.
    pub fn fork(&mut self) -> Self {
        Self::new(self.next_u64())
    }

    /// A uniformly random 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// A uniformly random 32-bit value.
    pub fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    /// A uniformly random boolean.
    pub fn next_bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Uniform value in `[0, n)`. Panics when `n == 0`.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below: empty range");
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform value in `[0, n)`. Panics when `n == 0`.
    pub fn below_u64(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below_u64: empty range");
        self.next_u64() % n
    }

    /// Uniform value in `[lo, hi]` inclusive. Panics when `lo > hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(lo <= hi, "range: lo ({lo}) > hi ({hi})");
        if lo == hi {
            return lo;
        }
        // Daniel Lemire's multiplication method: maps the random 64-bit
        // value into [0, span) using the high bits, with no modulo bias
        // and no overflow for any span (u128 arithmetic).
        let span = (hi as u128).wrapping_sub(lo as u128).wrapping_add(1);
        let x = self.next_u64() as u128;
        let r = (x.wrapping_mul(span) >> 64) as i64;
        lo.wrapping_add(r)
    }

    /// Uniform value in `[lo, hi]` inclusive over `usize`.
    pub fn range_usize(&mut self, lo: usize, hi: usize) -> usize {
        self.range(lo as i64, hi as i64) as usize
    }

    /// True with probability `p`, which must lie in `[0.0, 1.0]`.
    pub fn chance(&mut self, p: f64) -> bool {
        debug_assert!(
            (0.0..=1.0).contains(&p),
            "chance: probability out of range: {p}"
        );
        (self.next_u64() as f64) < p * (u64::MAX as f64)
    }

    /// Uniform random element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        assert!(!xs.is_empty(), "pick: empty slice");
        &xs[self.below(xs.len())]
    }

    /// Index into `weights`, proportional to the (non-negative) weights.
    /// Zero-weight entries are never picked. Panics when every weight is
    /// zero or the slice is empty.
    pub fn pick_index_weighted(&mut self, weights: &[u64]) -> usize {
        let total: u128 = weights.iter().map(|&w| w as u128).sum();
        assert!(total > 0, "pick_index_weighted: no positive weight");
        let mut r = (self.next_u64() as u128 * total) >> 64;
        // r lies in [0, total), so some bucket must catch it.
        for (i, &w) in weights.iter().enumerate() {
            let w = w as u128;
            if r < w {
                return i;
            }
            r -= w;
        }
        weights.len() - 1
    }

    /// In-place Fisher-Yates shuffle.
    pub fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.below(i + 1);
            xs.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinism() {
        let mut a = Rng::new(12345);
        let mut b = Rng::new(12345);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
            assert_eq!(a.next_bool(), b.next_bool());
            assert_eq!(a.below(17), b.below(17));
            assert_eq!(a.range(-100, 100), b.range(-100, 100));
            assert_eq!(a.chance(0.3), b.chance(0.3));
            assert_eq!(
                a.pick_index_weighted(&[1, 2, 3, 0]),
                b.pick_index_weighted(&[1, 2, 3, 0])
            );
        }
        let mut va = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let mut vb = va.clone();
        a.shuffle(&mut va);
        b.shuffle(&mut vb);
        assert_eq!(va, vb);
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let same = (0..100).filter(|_| a.next_u64() == b.next_u64()).count();
        assert!(same < 5, "streams suspiciously similar: {same}/100 equal");
    }

    #[test]
    fn range_bounds() {
        let mut rng = Rng::new(7);
        for _ in 0..10000 {
            let v = rng.range(-5, 5);
            assert!((-5..=5).contains(&v));
        }
        // Full-range extremes must not panic or overflow.
        let mut rng = Rng::new(8);
        for _ in 0..1000 {
            let _ = rng.range(i64::MIN, i64::MAX);
        }
        assert_eq!(rng.range(42, 42), 42);
    }

    #[test]
    fn zero_weight_never_picked() {
        let mut rng = Rng::new(99);
        for _ in 0..10000 {
            assert_ne!(rng.pick_index_weighted(&[5, 0, 3]), 1);
        }
    }

    #[test]
    fn shuffle_is_permutation() {
        let mut rng = Rng::new(3);
        let mut v: Vec<i32> = (0..50).collect();
        rng.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn fork_is_independent_and_deterministic() {
        let mut a = Rng::new(11);
        let mut a_child = a.fork();
        let mut b = Rng::new(11);
        let mut b_child = b.fork();
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
            assert_eq!(a_child.next_u64(), b_child.next_u64());
        }
    }
}
