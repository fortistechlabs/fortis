//! Folding extra entropy into seed generation.
//!
//! The platform CSPRNG (`crypto.getRandomValues`, `SecureRandom`, `getrandom`)
//! is already 256-bit and almost certainly sound. This is defence in depth: the
//! shell may collect additional entropy — pointer/touch jitter, device-motion
//! sensor noise, timing jitter, dice rolls — and this mixes it with the CSPRNG
//! bytes so the seed stays unpredictable *even if the CSPRNG is not*.
//!
//! `mix_entropy` can only ever add entropy. With a good hash `H`, `H(good ‖
//! anything)` is still good, so a broken CSPRNG is survived as long as one other
//! source has real entropy; conversely a broken extra source can never weaken a
//! good CSPRNG. Sources are length-prefixed (so `["ab","c"]` and `["a","bc"]`
//! differ) and domain-separated.

use bitcoin::hashes::{sha512, Hash, HashEngine};

const DOMAIN: &[u8] = b"fortis/seed-entropy/v1";

/// Fold every source into 32 bytes suitable for [`crate::MasterKey::generate`].
/// Pass the OS CSPRNG bytes as the first source and any collected extras after.
pub fn mix_entropy(sources: &[&[u8]]) -> [u8; 32] {
    let mut eng = sha512::Hash::engine();
    eng.input(DOMAIN);
    for s in sources {
        eng.input(&(s.len() as u64).to_le_bytes());
        eng.input(s);
    }
    let full = sha512::Hash::from_engine(eng);
    let mut out = [0u8; 32];
    out.copy_from_slice(&full.as_byte_array()[..32]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_order_sensitive() {
        assert_eq!(mix_entropy(&[b"a", b"b"]), mix_entropy(&[b"a", b"b"]));
        assert_ne!(mix_entropy(&[b"a", b"b"]), mix_entropy(&[b"b", b"a"]));
    }

    #[test]
    fn boundaries_are_unambiguous() {
        // Length-prefixing keeps concatenations distinct.
        assert_ne!(mix_entropy(&[b"ab", b"c"]), mix_entropy(&[b"a", b"bc"]));
    }

    #[test]
    fn extra_sources_change_the_result_but_absence_still_works() {
        let csprng = [7u8; 32];
        let base = mix_entropy(&[&csprng]);
        let with_extra = mix_entropy(&[&csprng, b"dice: 4 2 6 1 5 3"]);
        assert_ne!(base, with_extra);
        // an all-zero "extra" (a dead sensor) doesn't collapse the result to a
        // constant — it still depends on the CSPRNG bytes
        assert_ne!(mix_entropy(&[&[0u8; 32], &[0u8; 64]]), [0u8; 32]);
        assert_ne!(mix_entropy(&[&csprng, &[0u8; 64]]), mix_entropy(&[&[9u8; 32], &[0u8; 64]]));
    }

    #[test]
    fn output_looks_high_entropy() {
        let out = mix_entropy(&[&[1u8; 32], b"x"]);
        let ones: u32 = out.iter().map(|b| b.count_ones()).sum();
        assert!((96..160).contains(&ones), "popcount {ones} — not balanced");
    }
}
