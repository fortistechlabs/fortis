//! `fortis-index verify`: compare the UTXOs of a random sample of indexed
//! addresses against the node's own UTXO set (`scantxoutset`). Opens the
//! database as a RocksDB secondary, so it runs alongside the live service.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::hashes::Hash;
use bitcoin::{Address, Network, OutPoint, ScriptBuf, WPubkeyHash};
use serde_json::json;

use fortis_node::Rpc;

use crate::chain::HeaderFormat;
use crate::db::{Db, DbConfig, Reader};
use crate::keys::Program;

#[derive(Debug, PartialEq)]
pub enum Mismatch {
    /// The node has this UTXO; the index does not.
    Missing { address: String, outpoint: OutPoint },
    /// The index has this UTXO; the node does not.
    Extra { address: String, outpoint: OutPoint },
    Value {
        address: String,
        outpoint: OutPoint,
        ours: u64,
        node: u64,
    },
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mismatch::Missing { address, outpoint } => {
                write!(f, "{address}: missing {outpoint} (node has it)")
            }
            Mismatch::Extra { address, outpoint } => {
                write!(f, "{address}: extra {outpoint} (node does not have it)")
            }
            Mismatch::Value {
                address,
                outpoint,
                ours,
                node,
            } => {
                write!(f, "{address}: {outpoint} value {ours} ≠ node {node}")
            }
        }
    }
}

pub fn diff_utxos(
    address: &str,
    ours: &[(OutPoint, u64)],
    node: &[(OutPoint, u64)],
) -> Vec<Mismatch> {
    let ours: HashMap<OutPoint, u64> = ours.iter().copied().collect();
    let node: HashMap<OutPoint, u64> = node.iter().copied().collect();
    let mut out = Vec::new();
    for (op, &nv) in &node {
        match ours.get(op) {
            None => out.push(Mismatch::Missing {
                address: address.into(),
                outpoint: *op,
            }),
            Some(&ov) if ov != nv => out.push(Mismatch::Value {
                address: address.into(),
                outpoint: *op,
                ours: ov,
                node: nv,
            }),
            Some(_) => {}
        }
    }
    for op in ours.keys() {
        if !node.contains_key(op) {
            out.push(Mismatch::Extra {
                address: address.into(),
                outpoint: *op,
            });
        }
    }
    out.sort_by_key(|m| match m {
        Mismatch::Missing { outpoint, .. }
        | Mismatch::Extra { outpoint, .. }
        | Mismatch::Value { outpoint, .. } => *outpoint,
    });
    out
}

/// SplitMix64: a tiny deterministic generator (no `rand` dependency).
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Up to `n` distinct indexed programs: half from random seeks into `utxo`
/// (addresses holding coins), half into `history` (any activity, including
/// fully spent addresses — whose node UTXO set must then be empty too).
pub fn sample_programs(r: &Reader, n: usize, rng_seed: u64) -> Result<Vec<Program>> {
    let mut rng = SplitMix(rng_seed);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for i in 0..n.saturating_mul(20) {
        if out.len() == n {
            break;
        }
        let mut key = [0u8; 20];
        for chunk in key.chunks_mut(8) {
            chunk.copy_from_slice(&rng.next().to_be_bytes()[..chunk.len()]);
        }
        let history = i % 2 == 1;
        let p = match r.program_at_or_after(history, &key)? {
            Some(p) => Some(p),
            None => r.program_at_or_after(history, &[0u8; 20])?,
        };
        if let Some(p) = p {
            if seen.insert(p) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

fn address(p: &Program, network: Network) -> String {
    let spk = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(*p));
    Address::from_script(&spk, network)
        .expect("p2wpkh is standard")
        .to_string()
}

/// Wait until the secondary's tip equals the node's tip.
fn caught_up(db: &Db, rpc: &Rpc) -> Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        db.catch_up()?;
        let ours = db.reader().tip()?.map(|(h, _)| h);
        let node = rpc
            .call("getblockcount", json!([]))?
            .as_u64()
            .ok_or_else(|| anyhow!("getblockcount"))?;
        if ours == Some(node as u32) {
            return Ok(node as u32);
        }
        if Instant::now() > deadline {
            bail!("index tip {ours:?} has not reached node tip {node} after 10 minutes — is the index following?");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

type NodeUtxos = HashMap<Program, Vec<(OutPoint, u64)>>;

/// Node UTXOs per program from one `scantxoutset`, and the height it scanned at.
fn node_utxos(rpc: &Rpc, programs: &[Program], network: Network) -> Result<(u32, NodeUtxos)> {
    let descs: Vec<String> = programs
        .iter()
        .map(|p| format!("addr({})", address(p, network)))
        .collect();
    let res = rpc
        .call("scantxoutset", json!(["start", descs]))
        .context("scantxoutset")?;
    let height = res["height"]
        .as_u64()
        .ok_or_else(|| anyhow!("scantxoutset: no height in {res}"))? as u32;
    let mut out: HashMap<Program, Vec<(OutPoint, u64)>> = HashMap::new();
    for u in res["unspents"]
        .as_array()
        .ok_or_else(|| anyhow!("scantxoutset: no unspents"))?
    {
        let spk = hex::decode(u["scriptPubKey"].as_str().unwrap_or_default())
            .context("scriptPubKey hex")?;
        if spk.len() != 22 || spk[..2] != [0x00, 0x14] {
            bail!("scantxoutset returned a non-P2WPKH output: {u}");
        }
        let p: Program = spk[2..].try_into().unwrap();
        let txid = u["txid"]
            .as_str()
            .ok_or_else(|| anyhow!("unspent txid"))?
            .parse()?;
        let vout = u["vout"].as_u64().ok_or_else(|| anyhow!("unspent vout"))? as u32;
        let sats = (u["amount"]
            .as_f64()
            .ok_or_else(|| anyhow!("unspent amount"))?
            * 1e8)
            .round() as u64;
        out.entry(p)
            .or_default()
            .push((OutPoint { txid, vout }, sats));
    }
    Ok((height, out))
}

fn check(
    db: &Db,
    rpc: &Rpc,
    network: Network,
    sample: usize,
    seed: u64,
) -> Result<Option<Vec<Mismatch>>> {
    let tip = caught_up(db, rpc)?;
    let r = db.reader();
    if r.tip()?.map(|(h, _)| h) != Some(tip) {
        return Ok(None);
    }
    let programs = sample_programs(&r, sample, seed)?;
    eprintln!(
        "verify: {} addresses at height {tip} (seed {seed})",
        programs.len()
    );
    let (scanned_at, node) = node_utxos(rpc, &programs, network)?;
    if scanned_at != tip {
        return Ok(None);
    }
    let mut out = Vec::new();
    for p in &programs {
        let ours: Vec<(OutPoint, u64)> = r
            .utxos(p, usize::MAX)?
            .into_iter()
            .map(|(op, u)| (op, u.value))
            .collect();
        let theirs = node.get(p).map(Vec::as_slice).unwrap_or_default();
        out.extend(diff_utxos(&address(p, network), &ours, theirs));
    }
    Ok(Some(out))
}

/// Compare `sample` addresses' UTXOs against the node. Retries (up to 3
/// times) when a new block lands during the scan.
pub fn run(db_path: &Path, rpc: &Rpc, network: Network, sample: usize) -> Result<Vec<Mismatch>> {
    let chain_id = HeaderFormat::detect(rpc)?.chain_id();
    let secondary =
        std::env::temp_dir().join(format!("fortis-index-verify-{}", std::process::id()));
    let db = Db::open_secondary(db_path, &secondary, &DbConfig { cache_mb: 256 }, &chain_id)?;
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64);
    let result = (|| {
        for _ in 0..3 {
            if let Some(m) = check(&db, rpc, network, sample, seed)? {
                return Ok(m);
            }
            eprintln!("verify: the chain moved during the scan; retrying");
        }
        bail!("the chain kept moving during 3 scans; try again")
    })();
    drop(db);
    let _ = std::fs::remove_dir_all(&secondary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Durability;
    use crate::extract::{Funded, ParsedBlock, TxRows};
    use bitcoin::{BlockHash, Txid};

    fn op(n: u8, vout: u32) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout,
        }
    }

    #[test]
    fn diff_finds_missing_extra_and_value_mismatches() {
        let ours = [(op(1, 0), 10), (op(2, 0), 20), (op(3, 0), 30)];
        let node = [(op(1, 0), 10), (op(2, 0), 21), (op(4, 0), 40)];
        assert_eq!(
            diff_utxos("bc1qx", &ours, &node),
            vec![
                Mismatch::Value {
                    address: "bc1qx".into(),
                    outpoint: op(2, 0),
                    ours: 20,
                    node: 21
                },
                Mismatch::Extra {
                    address: "bc1qx".into(),
                    outpoint: op(3, 0)
                },
                Mismatch::Missing {
                    address: "bc1qx".into(),
                    outpoint: op(4, 0)
                },
            ]
        );
        assert!(diff_utxos("a", &ours, &ours).is_empty());
        assert!(diff_utxos("a", &[], &[]).is_empty());
    }

    #[test]
    fn sample_programs_is_deterministic_for_a_seed_and_unique() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let txs = (0..200u32)
            .map(|i| {
                let mut p = [0u8; 20];
                p[..4].copy_from_slice(&i.wrapping_mul(2_654_435_761).to_be_bytes());
                TxRows {
                    txid: Txid::from_byte_array([i as u8; 32]),
                    funded: vec![Funded {
                        program: p,
                        vout: i,
                        value: 1,
                    }],
                    spent: vec![],
                }
            })
            .collect();
        let b = ParsedBlock {
            height: 1,
            hash: BlockHash::all_zeros(),
            prev: BlockHash::all_zeros(),
            time: 0,
            txs,
        };
        db.apply(&[b], 0, Durability::Durable, false).unwrap();
        let r = db.reader();
        let a = sample_programs(&r, 50, 7).unwrap();
        assert_eq!(a, sample_programs(&r, 50, 7).unwrap());
        assert_ne!(a, sample_programs(&r, 50, 8).unwrap());
        assert_eq!(a.len(), 50);
        assert_eq!(a.iter().collect::<HashSet<_>>().len(), 50);
        // Asking for more than exist returns every program once.
        assert_eq!(sample_programs(&r, 500, 7).unwrap().len(), 200);
        let empty = tempfile::tempdir().unwrap();
        let db = Db::open(empty.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        assert!(sample_programs(&db.reader(), 5, 1).unwrap().is_empty());
    }

    #[test]
    fn a_secondary_sees_the_primarys_writes_after_catch_up() {
        let dir = tempfile::tempdir().unwrap();
        let sec = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        let s =
            Db::open_secondary(dir.path(), sec.path(), &DbConfig { cache_mb: 8 }, "btc").unwrap();
        assert_eq!(s.reader().tip().unwrap(), None);
        let b = ParsedBlock {
            height: 5,
            hash: BlockHash::all_zeros(),
            prev: BlockHash::all_zeros(),
            time: 0,
            txs: vec![TxRows {
                txid: Txid::all_zeros(),
                funded: vec![Funded {
                    program: [1; 20],
                    vout: 0,
                    value: 9,
                }],
                spent: vec![],
            }],
        };
        db.apply(&[b], 0, Durability::Durable, false).unwrap();
        s.catch_up().unwrap();
        assert_eq!(s.reader().tip().unwrap().map(|t| t.0), Some(5));
        assert!(
            Db::open_secondary(dir.path(), sec.path(), &DbConfig { cache_mb: 8 }, "xbt@1").is_err()
        );
    }
}
