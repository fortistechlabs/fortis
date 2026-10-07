//! On-disk key/value encodings for the v2 index. All integers are big-endian so
//! RocksDB's bytewise ordering matches numeric ordering.

use anyhow::{bail, Result};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, OutPoint, Txid};

pub type Program = [u8; 20];
pub type TxNum = u64;
pub const TXNUM_LEN: usize = 5;
pub const MAX_TXNUM: TxNum = (1 << 40) - 1;

/// 5-byte big-endian transaction number. Panics above `MAX_TXNUM`.
pub fn txnum_key(n: TxNum) -> [u8; 5] {
    assert!(n <= MAX_TXNUM, "txnum {n} exceeds 40 bits");
    let b = n.to_be_bytes();
    [b[3], b[4], b[5], b[6], b[7]]
}

/// Decode the first 5 bytes of `b` as a txnum.
pub fn txnum_from(b: &[u8]) -> TxNum {
    let mut a = [0u8; 8];
    a[3..].copy_from_slice(&b[..TXNUM_LEN]);
    u64::from_be_bytes(a)
}

pub fn height_key(h: u32) -> [u8; 4] {
    h.to_be_bytes()
}

/// `history` key: program · txnum.
pub fn history_key(p: &Program, n: TxNum) -> [u8; 25] {
    let mut k = [0u8; 25];
    k[..20].copy_from_slice(p);
    k[20..].copy_from_slice(&txnum_key(n));
    k
}

/// The txnum of a `history` key (its last 5 bytes).
pub fn history_txnum(key: &[u8]) -> TxNum {
    txnum_from(&key[20..])
}

/// `utxo` key: program · txid (internal byte order) · vout BE.
pub fn utxo_key(p: &Program, op: &OutPoint) -> [u8; 56] {
    let mut k = [0u8; 56];
    k[..20].copy_from_slice(p);
    k[20..52].copy_from_slice(&op.txid.to_byte_array());
    k[52..].copy_from_slice(&op.vout.to_be_bytes());
    k
}

pub fn utxo_outpoint(key: &[u8]) -> OutPoint {
    let txid: [u8; 32] = key[20..52].try_into().unwrap();
    let vout = u32::from_be_bytes(key[52..56].try_into().unwrap());
    OutPoint { txid: Txid::from_byte_array(txid), vout }
}

/// `utxo` value: value u64 BE · height u32 BE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UtxoVal {
    pub value: u64,
    pub height: u32,
}

impl UtxoVal {
    pub fn encode(&self) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..8].copy_from_slice(&self.value.to_be_bytes());
        b[8..].copy_from_slice(&self.height.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 12 {
            bail!("utxo value: expected 12 bytes, got {}", b.len());
        }
        Ok(Self {
            value: u64::from_be_bytes(b[..8].try_into().unwrap()),
            height: u32::from_be_bytes(b[8..].try_into().unwrap()),
        })
    }
}

/// `blocks` value: hash 32 · time u32 · first_txnum 5 · n_txs u32 (all BE).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRec {
    pub hash: BlockHash,
    pub time: u32,
    pub first_txnum: TxNum,
    pub n_txs: u32,
}

impl BlockRec {
    pub fn encode(&self) -> [u8; 45] {
        let mut b = [0u8; 45];
        b[..32].copy_from_slice(&self.hash.to_byte_array());
        b[32..36].copy_from_slice(&self.time.to_be_bytes());
        b[36..41].copy_from_slice(&txnum_key(self.first_txnum));
        b[41..].copy_from_slice(&self.n_txs.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != 45 {
            bail!("block record: expected 45 bytes, got {}", b.len());
        }
        Ok(Self {
            hash: BlockHash::from_byte_array(b[..32].try_into().unwrap()),
            time: u32::from_be_bytes(b[32..36].try_into().unwrap()),
            first_txnum: txnum_from(&b[36..41]),
            n_txs: u32::from_be_bytes(b[41..].try_into().unwrap()),
        })
    }
}

/// `meta` tip value: height u32 BE · hash 32.
pub fn tip_value(h: u32, hash: &BlockHash) -> [u8; 36] {
    let mut b = [0u8; 36];
    b[..4].copy_from_slice(&h.to_be_bytes());
    b[4..].copy_from_slice(&hash.to_byte_array());
    b
}

pub fn tip_from(b: &[u8]) -> Result<(u32, BlockHash)> {
    if b.len() != 36 {
        bail!("tip: expected 36 bytes, got {}", b.len());
    }
    let h = u32::from_be_bytes(b[..4].try_into().unwrap());
    Ok((h, BlockHash::from_byte_array(b[4..].try_into().unwrap())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txnum_round_trips_at_the_edges() {
        for n in [0, 1, 255, 1 << 32, MAX_TXNUM] {
            assert_eq!(txnum_from(&txnum_key(n)), n);
        }
    }

    #[test]
    #[should_panic]
    fn txnum_above_40_bits_panics() {
        txnum_key(MAX_TXNUM + 1);
    }

    #[test]
    fn history_keys_sort_by_program_then_txnum() {
        let (a, b) = ([1u8; 20], [2u8; 20]);
        assert!(history_key(&a, 2) > history_key(&a, 1));
        assert!(history_key(&b, 0) > history_key(&a, MAX_TXNUM));
        assert_eq!(history_txnum(&history_key(&a, 77)), 77);
    }

    #[test]
    fn utxo_key_round_trips_the_outpoint() {
        let p = [7u8; 20];
        let op = OutPoint { txid: Txid::from_byte_array([9; 32]), vout: 70000 };
        let k = utxo_key(&p, &op);
        assert_eq!(utxo_outpoint(&k), op);
        assert_eq!(k[..20], p);
    }

    #[test]
    fn block_rec_and_utxo_val_round_trip() {
        let v = UtxoVal { value: 123_456_789_012, height: 976_000 };
        assert_eq!(UtxoVal::decode(&v.encode()).unwrap(), v);
        assert!(UtxoVal::decode(&[1, 2, 3]).is_err());

        let r = BlockRec {
            hash: BlockHash::from_byte_array([5; 32]),
            time: 1_700_000_000,
            first_txnum: (1 << 33) + 5,
            n_txs: 3000,
        };
        assert_eq!(BlockRec::decode(&r.encode()).unwrap(), r);
        assert!(BlockRec::decode(&[1, 2, 3]).is_err());
    }

    #[test]
    fn tip_value_round_trips() {
        let h = BlockHash::from_byte_array([0xab; 32]);
        assert_eq!(tip_from(&tip_value(976_000, &h)).unwrap(), (976_000, h));
        assert!(tip_from(&[0; 5]).is_err());
    }
}
