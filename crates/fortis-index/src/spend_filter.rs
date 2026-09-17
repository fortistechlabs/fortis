//! A Bloom filter answering one question fast: "have we ever stored an
//! output for this (txid, vout)?" It exists purely to skip a real database
//! round-trip for the overwhelming majority of transaction inputs, which
//! spend an output this index never tracked in the first place (P2WPKH is a
//! small slice of all on-chain outputs, and every input from every
//! transaction is checked, not just ones that turn out to matter).
//!
//! Bloom filters never produce false negatives, only false positives: a
//! `false` answer here is a hard guarantee the real table has no matching
//! row, safe to skip. A `true` answer just means "check the table" (which
//! `Store::apply_blocks` always still does) -- so this can never cause a
//! missed spend, only an occasional unnecessary lookup.

pub struct SpendFilter {
    bits: Vec<u64>,
    num_bits: u64,
    k: u32,
}

impl SpendFilter {
    /// Sized for `expected_items` at roughly a 1% false-positive rate
    /// (~9.6 bits/item, the standard optimum-`k` Bloom sizing).
    pub fn new(expected_items: u64) -> Self {
        let expected_items = expected_items.max(1);
        let num_bits = ((expected_items as f64) * 9.6).ceil() as u64;
        let num_bits = num_bits.max(64);
        let words = (num_bits as usize).div_ceil(64);
        Self { bits: vec![0u64; words], num_bits: (words * 64) as u64, k: 7 }
    }

    fn hashes(txid: &str, vout: u32) -> (u64, u64) {
        use std::hash::{Hash, Hasher};
        let mut h1 = std::collections::hash_map::DefaultHasher::new();
        txid.hash(&mut h1);
        vout.hash(&mut h1);
        let a = h1.finish();
        // A second, independent-enough hash via a differently-perturbed
        // input (Kirsch-Mitzenmacher: two real hashes combine into `k`).
        let mut h2 = std::collections::hash_map::DefaultHasher::new();
        0x9e3779b97f4a7c15u64.hash(&mut h2);
        vout.hash(&mut h2);
        txid.hash(&mut h2);
        let b = h2.finish() | 1; // odd, so repeated addition can't cycle short on a power-of-two-ish range
        (a, b)
    }

    pub fn insert(&mut self, txid: &str, vout: u32) {
        let (a, b) = Self::hashes(txid, vout);
        for i in 0..self.k as u64 {
            let bit = a.wrapping_add(i.wrapping_mul(b)) % self.num_bits;
            self.bits[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// `false` means definitely absent (safe to skip the DB check); `true`
    /// means maybe present (must still check).
    pub fn might_contain(&self, txid: &str, vout: u32) -> bool {
        let (a, b) = Self::hashes(txid, vout);
        (0..self.k as u64).all(|i| {
            let bit = a.wrapping_add(i.wrapping_mul(b)) % self.num_bits;
            self.bits[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_inserted_item_is_reported_as_present() {
        let mut f = SpendFilter::new(1000);
        for i in 0..1000u32 {
            f.insert(&format!("{i:064x}"), i % 4);
        }
        for i in 0..1000u32 {
            assert!(f.might_contain(&format!("{i:064x}"), i % 4), "false negative at {i}");
        }
    }

    #[test]
    fn a_freshly_built_filter_holds_nothing() {
        let f = SpendFilter::new(1000);
        assert!(!f.might_contain("a".repeat(64).as_str(), 0));
    }

    #[test]
    fn false_positive_rate_stays_low_at_the_sized_capacity() {
        let mut f = SpendFilter::new(1000);
        for i in 0..1000u32 {
            f.insert(&format!("in-{i:064x}"), i % 4);
        }
        let false_positives =
            (0..5000u32).filter(|i| f.might_contain(&format!("out-{i:064x}"), i % 4)).count();
        // Sized for ~1% FPR; generous bound to keep this non-flaky.
        assert!(false_positives < 250, "false positive rate too high: {false_positives}/5000");
    }
}
