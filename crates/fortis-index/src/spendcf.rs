//! The `spends` column family: outpoint → the input that spent it, for the
//! outputs Lightning cares about (channel funding outputs and their HTLC and
//! commitment descendants). A phone asks "who spent this funding output",
//! however old the channel; this answers from one point lookup instead of a
//! forward block scan.
//!
//! Only inputs that can spend such an output are recorded: a single-key spend
//! (legacy, P2WPKH, Taproot key path) never spends a 2-of-2 or a script, so
//! those are left out — most of the chain's inputs.

use bitcoin::hashes::Hash;
use bitcoin::{OutPoint, Transaction, TxIn, Txid};

/// Whether `input`'s spend is recorded. True unless the witness is empty
/// (legacy), exactly two items with a 33-byte second (P2WPKH, bare or
/// P2SH-wrapped), or exactly one item of 64 or 65 bytes (Taproot key path).
pub fn recorded(input: &TxIn) -> bool {
    let w = &input.witness;
    match w.len() {
        0 => false,
        1 => !matches!(w.nth(0).map(<[u8]>::len), Some(64 | 65)),
        2 => w.nth(1).map(<[u8]>::len) != Some(33),
        _ => true,
    }
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

    #[allow(dead_code)] // read by the outspend lookup (Plan B Task 2)
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
