//! Raw block parsing for both chains the indexer follows: Bitcoin (80-byte
//! headers) and XBT, whose BLAKE2b fork uses 164-byte headers from its
//! activation height. Only `prev` (bytes 4..36) and `time` (LE u32 at 68..72)
//! are read from the header, and both sit at the same offsets in both formats;
//! transactions are standard Bitcoin encoding.

use anyhow::{bail, ensure, Context, Result};
use bitcoin::consensus::{encode::VarInt, Decodable};
use bitcoin::hashes::Hash;
use serde_json::Value;

/// Which header layout a chain uses at a given height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderFormat {
    /// 164-byte headers from this height; `None` for plain Bitcoin.
    pub v2_from: Option<u32>,
}

impl HeaderFormat {
    pub const BTC: HeaderFormat = HeaderFormat { v2_from: None };

    pub fn header_len(&self, height: u32) -> usize {
        match self.v2_from {
            Some(h) if height >= h => 164,
            _ => 80,
        }
    }

    /// Stable identifier for the chain, used to tell indexes apart.
    pub fn chain_id(&self) -> String {
        match self.v2_from {
            Some(h) => format!("xbt@{h}"),
            None => "btc".to_string(),
        }
    }

    /// Knots reports the fork under `deployments.blake2b.height`; anything
    /// without it is treated as plain Bitcoin.
    pub fn from_deployment_info(v: &Value) -> Self {
        let h = v["deployments"]["blake2b"]["height"]
            .as_u64()
            .and_then(|h| u32::try_from(h).ok());
        HeaderFormat { v2_from: h }
    }

    pub fn detect(rpc: &fortis_node::Rpc) -> Result<Self> {
        let info = rpc
            .call("getdeploymentinfo", serde_json::json!([]))
            .context("getdeploymentinfo")?;
        Ok(Self::from_deployment_info(&info))
    }
}

#[derive(Debug)]
pub struct BlockBody {
    pub prev: bitcoin::BlockHash,
    pub time: u32,
    pub txs: Vec<bitcoin::Transaction>,
}

/// Parse a raw block whose header is `header_len` bytes. A wrong `header_len`
/// or truncated/extra data is an error rather than misparsed transactions.
pub fn parse_block(bytes: &[u8], header_len: usize) -> Result<BlockBody> {
    ensure!(
        header_len >= 72 && bytes.len() >= header_len,
        "block too short: {} bytes for a {header_len}-byte header",
        bytes.len()
    );
    let prev = bitcoin::BlockHash::from_slice(&bytes[4..36]).context("prev hash")?;
    let time = u32::from_le_bytes(bytes[68..72].try_into().expect("4 bytes"));
    let mut r = &bytes[header_len..];
    let n = VarInt::consensus_decode(&mut r).context("tx count")?.0;
    // Each tx is at least 10 bytes; reject absurd counts before allocating.
    ensure!(
        n <= r.len() as u64 / 10,
        "implausible tx count {n} for {} remaining bytes (wrong header length?)",
        r.len()
    );
    let mut txs = Vec::with_capacity(n as usize);
    for i in 0..n {
        txs.push(
            bitcoin::Transaction::consensus_decode(&mut r)
                .with_context(|| format!("decoding tx {i} of {n}"))?,
        );
    }
    if !r.is_empty() {
        bail!("trailing bytes: {} after {n} txs", r.len());
    }
    Ok(BlockBody { prev, time, txs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use serde_json::json;

    fn fixture() -> Vec<u8> {
        hex::decode(include_str!("../tests/fixtures/xbt-976000.hex").trim()).unwrap()
    }

    #[test]
    fn parses_a_real_xbt_block_with_a_164_byte_header() {
        let b = parse_block(&fixture(), 164).unwrap();
        assert_eq!(
            b.prev.to_string(),
            "00000000000000007b33968e11ae9476536a8034c838d71bf7f32b2836b4d080"
        );
        assert_eq!(b.time, 1_791_360_298);
        assert_eq!(b.txs.len(), 45);
        assert!(b.txs[0].is_coinbase());
    }

    #[test]
    fn the_wrong_header_length_is_an_error_not_garbage() {
        assert!(parse_block(&fixture(), 80).is_err());
    }

    #[test]
    fn parses_a_standard_80_byte_block() {
        use bitcoin::{
            absolute::LockTime, block, transaction, Amount, Block, BlockHash, CompactTarget,
            OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxMerkleNode, TxOut, Txid, Witness,
        };
        let tx = |n: u8| Transaction {
            version: transaction::Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: if n == 0 {
                    OutPoint::null()
                } else {
                    OutPoint::new(Txid::from_byte_array([n; 32]), 0)
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1000 + n as u64),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let prev = BlockHash::from_byte_array([7; 32]);
        let block = Block {
            header: block::Header {
                version: block::Version::TWO,
                prev_blockhash: prev,
                merkle_root: TxMerkleNode::from_byte_array([9; 32]),
                time: 1_700_000_000,
                bits: CompactTarget::from_consensus(0x1d00ffff),
                nonce: 1,
            },
            txdata: vec![tx(0), tx(1)],
        };
        let raw = bitcoin::consensus::serialize(&block);
        let b = parse_block(&raw, 80).unwrap();
        assert_eq!(b.prev, prev);
        assert_eq!(b.time, 1_700_000_000);
        assert_eq!(b.txs.len(), 2);
        assert_eq!(b.txs[0].compute_txid(), block.txdata[0].compute_txid());
        assert_eq!(b.txs[1].compute_txid(), block.txdata[1].compute_txid());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut raw = fixture();
        raw.push(0);
        let e = parse_block(&raw, 164).unwrap_err().to_string();
        assert!(e.contains("trailing"), "{e}");
    }

    #[test]
    fn header_format_from_deployment_info() {
        let knots = json!({"deployments": {"blake2b": {"height": 961640, "active": true}}});
        let f = HeaderFormat::from_deployment_info(&knots);
        assert_eq!((f.header_len(961639), f.header_len(961640)), (80, 164));
        assert_eq!(f.chain_id(), "xbt@961640");
        let core = json!({"deployments": {"taproot": {"height": 709632, "active": true}}});
        assert_eq!(HeaderFormat::from_deployment_info(&core), HeaderFormat::BTC);
        assert_eq!(HeaderFormat::BTC.chain_id(), "btc");
    }
}
