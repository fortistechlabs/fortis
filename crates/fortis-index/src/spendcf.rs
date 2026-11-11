//! The `spends` column family: outpoint → the input that spent it, for the
//! outputs Lightning cares about (channel funding outputs and their HTLC and
//! commitment descendants). A phone asks "who spent this funding output",
//! however old the channel; this answers from one point lookup instead of a
//! forward block scan.
//!
//! Only inputs that can spend such an output are recorded: a single-key spend
//! (legacy, P2WPKH, Taproot key path) never spends a 2-of-2 or a script, and a
//! Taproot script-path spend never spends a P2WSH output (the only kind this
//! answers for), so those are left out — most of the chain's inputs.

use bitcoin::hashes::Hash;
use bitcoin::{OutPoint, Transaction, TxIn, Txid};

/// Whether `input`'s spend is recorded. True unless the witness is empty
/// (legacy), exactly two items with a 33-byte second (P2WPKH, bare or
/// P2SH-wrapped), or — once a Taproot annex (a last item starting 0x50, of
/// two or more) is set aside — exactly one item of 64 or 65 bytes (Taproot
/// key path) or a last item that is a Taproot control block (script path).
/// The column family only answers for P2WSH outputs, which are none of these.
pub fn recorded(input: &TxIn) -> bool {
    let w = &input.witness;
    match w.len() {
        0 => return false,
        2 if w.nth(1).map(<[u8]>::len) == Some(33) => return false,
        _ => {}
    }
    let annexed = w.len() >= 2 && w.last().is_some_and(|a| a.first() == Some(&0x50));
    let n = w.len() - usize::from(annexed);
    if n == 1 && matches!(w.nth(0).map(<[u8]>::len), Some(64 | 65)) {
        return false;
    }
    !(n >= 2 && w.nth(n - 1).is_some_and(control_block))
}

/// A Taproot control block: a leaf-version byte (0xc0 or 0xc1, the
/// tapscript leaf with either parity), the 32-byte internal key, then zero or
/// more 32-byte Merkle path steps.
fn control_block(c: &[u8]) -> bool {
    c.len() >= 33 && (c.len() - 33) % 32 == 0 && c[0] & 0xfe == 0xc0
}

/// One recorded spend in a block: input `vin` of `txid` spends `prevout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSpend {
    pub prevout: OutPoint,
    pub txid: Txid,
    pub vin: u32,
}

/// The recorded spends of `tx` (whose txid is `txid`); none for a coinbase.
pub fn tx_spends(tx: &Transaction, txid: Txid) -> impl Iterator<Item = BlockSpend> + '_ {
    let inputs = if tx.is_coinbase() {
        &[][..]
    } else {
        &tx.input[..]
    };
    inputs
        .iter()
        .enumerate()
        .filter(|(_, i)| recorded(i))
        .map(move |(vin, i)| BlockSpend {
            prevout: i.previous_output,
            txid,
            vin: vin as u32,
        })
}

/// A `spends` row's value: the spending transaction, its input, its height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpendRec {
    pub txid: Txid,
    pub vin: u32,
    pub height: u32,
}

impl SpendRec {
    /// txid (32) · vin (4, BE) · height (4, BE).
    pub fn encode(&self) -> [u8; 40] {
        let mut b = [0u8; 40];
        b[..32].copy_from_slice(&self.txid.to_byte_array());
        b[32..36].copy_from_slice(&self.vin.to_be_bytes());
        b[36..].copy_from_slice(&self.height.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<SpendRec> {
        let b: &[u8; 40] = b.try_into().ok()?;
        Some(SpendRec {
            txid: Txid::from_byte_array(b[..32].try_into().unwrap()),
            vin: u32::from_be_bytes(b[32..36].try_into().unwrap()),
            height: u32::from_be_bytes(b[36..].try_into().unwrap()),
        })
    }
}

/// A `spends` row's key: the spent output's txid bytes, then its vout (BE).
pub fn key(op: &OutPoint) -> [u8; 36] {
    let mut k = [0u8; 36];
    k[..32].copy_from_slice(&op.txid.to_byte_array());
    k[32..].copy_from_slice(&op.vout.to_be_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::{transaction::Version, ScriptBuf, Sequence, Witness};

    fn input(items: &[Vec<u8>]) -> TxIn {
        TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([9; 32]), 3),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(items),
        }
    }

    #[test]
    fn recorded_skips_p2wpkh_taproot_keypath_and_legacy() {
        let legacy = TxIn {
            script_sig: ScriptBuf::from_bytes(vec![1, 2]),
            ..input(&[])
        };
        assert!(!recorded(&legacy));
        assert!(!recorded(&input(&[vec![0; 72], vec![2; 33]])), "P2WPKH");
        assert!(!recorded(&input(&[vec![0; 64]])), "Taproot key path");
        assert!(
            !recorded(&input(&[vec![0; 65]])),
            "Taproot key path, sighash byte"
        );
        let two_of_two = input(&[vec![], vec![0; 72], vec![0; 71], vec![0x52; 71]]);
        assert!(recorded(&two_of_two), "P2WSH 2-of-2");
        // Near misses of the skip rules are recorded.
        assert!(
            recorded(&input(&[vec![0; 72], vec![0; 40]])),
            "2 items, script"
        );
        assert!(recorded(&input(&[vec![0; 63]])), "1 item, not a signature");
    }

    /// A Taproot script-path spend ends in its control block (33 + 32k bytes,
    /// leaf version byte 0xc0/0xc1), or — with an annex (last item starting
    /// 0x50) — has it second to last. The spends column family only answers
    /// for P2WSH outputs, so those are skipped; the P2WSH shapes Lightning
    /// spends (2-of-2, HTLC, to_local, anchor) stay recorded.
    #[test]
    fn recorded_skips_taproot_script_path_and_keeps_p2wsh_shapes() {
        let control = |k: usize, leaf: u8| {
            let mut c = vec![0x02; 33 + 32 * k];
            c[0] = leaf;
            c
        };
        let script = vec![0x20; 34];
        for k in [0, 1, 2, 5] {
            for leaf in [0xc0, 0xc1] {
                assert!(
                    !recorded(&input(&[vec![0; 64], script.clone(), control(k, leaf)])),
                    "script path, k={k}, leaf {leaf:#x}"
                );
                let annex = vec![0x50, 1, 2];
                assert!(
                    !recorded(&input(&[vec![0; 64], script.clone(), control(k, leaf), annex])),
                    "script path with annex, k={k}"
                );
            }
        }
        // Two items: a script and its control block (no signature needed).
        assert!(!recorded(&input(&[script.clone(), control(0, 0xc0)])), "2-item script path");
        // Key path with an annex: a single-key spend.
        assert!(!recorded(&input(&[vec![0; 64], vec![0x50]])), "key path, annex");
        assert!(!recorded(&input(&[vec![0; 65], vec![0x50, 9]])), "key path, sighash, annex");

        // Not a control block: wrong length or leaf byte.
        assert!(recorded(&input(&[vec![0; 64], script.clone(), vec![0xc0; 34]])), "length 34");
        assert!(recorded(&input(&[vec![0; 64], script.clone(), control(1, 0xc2)])), "leaf 0xc2");
        assert!(recorded(&input(&[vec![0; 64], script.clone(), control(1, 0xa0)])), "leaf 0xa0");

        // P2WSH 2-of-2 (funding): empty, sig, sig, 71-byte script (OP_2 ...).
        let funding = vec![0x52; 71];
        assert!(recorded(&input(&[vec![], vec![0; 72], vec![0; 71], funding.clone()])), "2-of-2");
        // A 2-of-2 whose script happens to start 0x50 is still a P2WSH.
        assert!(recorded(&input(&[vec![], vec![0; 72], vec![0; 71], vec![0x52; 71], vec![0x50; 71]])));
        // to_local: sig, empty (or 1), script starting OP_IF.
        let to_local = vec![0x63; 79];
        assert!(recorded(&input(&[vec![0; 72], vec![], to_local.clone()])), "to_local delayed");
        assert!(recorded(&input(&[vec![0; 72], vec![1], to_local])), "to_local revoked");
        // Offered / received HTLC: scripts starting OP_DUP.
        let htlc = vec![0x76; 133];
        assert!(recorded(&input(&[vec![], vec![0; 72], vec![0; 72], vec![], htlc.clone()])), "HTLC timeout");
        assert!(recorded(&input(&[vec![], vec![0; 72], vec![0; 72], vec![7; 32], htlc.clone()])), "HTLC success");
        assert!(recorded(&input(&[vec![0; 72], vec![2; 33], htlc])), "HTLC revoked");
        // Anchor: sig (or empty after 16 blocks) and the 40-byte script.
        let anchor = vec![0x21; 40];
        assert!(recorded(&input(&[vec![0; 72], anchor.clone()])), "anchor");
        assert!(recorded(&input(&[vec![], anchor])), "anchor, anyone after 16 blocks");
    }

    #[test]
    fn tx_spends_numbers_inputs_and_skips_coinbase() {
        let t = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![
                input(&[vec![0; 72], vec![2; 33]]),
                input(&[vec![], vec![0; 72], vec![0; 72], vec![0x52; 71]]),
            ],
            output: vec![],
        };
        let txid = Txid::from_byte_array([5; 32]);
        let got: Vec<_> = tx_spends(&t, txid).collect();
        assert_eq!(
            got,
            vec![BlockSpend {
                prevout: OutPoint::new(Txid::from_byte_array([9; 32]), 3),
                txid,
                vin: 1
            }]
        );
        let coinbase = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                ..input(&[vec![0; 32]])
            }],
            ..t
        };
        assert_eq!(tx_spends(&coinbase, txid).count(), 0);
    }

    #[test]
    fn spend_rec_roundtrip() {
        let r = SpendRec {
            txid: Txid::from_byte_array([7; 32]),
            vin: 0x0102_0304,
            height: 961_640,
        };
        assert_eq!(SpendRec::decode(&r.encode()), Some(r));
        assert_eq!(SpendRec::decode(&r.encode()[..39]), None);
        let op = OutPoint::new(Txid::from_byte_array([1; 32]), 0x0a0b);
        let k = key(&op);
        assert_eq!(&k[..32], &[1; 32]);
        assert_eq!(&k[32..], &[0, 0, 0x0a, 0x0b]);
    }
}
