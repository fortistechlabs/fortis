//! The rest of the Esplora API that ldk-node's chain source (`esplora-client`
//! via `bdk_esplora` and `lightning-transaction-sync`) calls: fee estimates,
//! block hashes / headers / status / summaries, raw transactions and their
//! status, merkle-block proofs, and a script's history by script hash. Shapes
//! follow Blockstream's Esplora exactly, as `esplora-client` deserialises them.
//!
//! Everything but the script-hash history comes from the node; block hashes
//! by height and the tip come from the index's own chain, so they agree with
//! `/blocks/tip/height` and the histories.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bitcoin::{BlockHash, Txid};
use serde_json::{json, Map, Value};

use fortis_node::Rpc;

use super::{on_node, parse_txid, text, unknown_tx, with_view, ApiError, ApiResult, AppState, PENDING_MAX};
use crate::keys::TxNum;
use crate::render::btc_to_sat;

/// Confirmed transactions per `/scripthash/:sh/txs[/chain/:last]` page
/// (Esplora's; `bdk_esplora` stops paging at a shorter page).
pub const CHAIN_PAGE: usize = 25;
/// Mempool transactions on the first page (Esplora's).
pub const MEMPOOL_PAGE: usize = 50;
/// Most history rows read for one script (paging walks this list).
pub const MAX_SCRIPT_HISTORY: usize = 10_000;
/// Blocks per `/blocks[/:height]` answer (Esplora's).
pub const BLOCKS_PAGE: u32 = 10;
/// Confirmation targets `/fee-estimates` answers for (Esplora's set).
pub const FEE_TARGETS: [u16; 11] = [1, 2, 3, 4, 5, 6, 10, 20, 144, 504, 1008];
/// Block summaries kept in memory (they never change once a hash is fixed).
const BLOCK_CACHE_MAX: usize = 256;

fn not_found(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("error -5") || s.contains("error -8")
}

fn missing(what: &str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn parse_hash(s: &str) -> ApiResult<BlockHash> {
    s.parse().map_err(|e| ApiError::bad_request(format!("bad block hash: {e}")))
}

/// An RPC answer, `None` when the node says it doesn't know the object.
fn call_opt(rpc: &Rpc, method: &str, params: Value) -> ApiResult<Option<Value>> {
    match rpc.call(method, params) {
        Ok(v) => Ok(Some(v)),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(ApiError::node(e)),
    }
}

// ---- fee estimates -------------------------------------------------------

/// `GET /fee-estimates` — `{"<target>": sat/vB, …}` from `estimatesmartfee`.
/// A target the node can't estimate takes the next faster target's rate (or
/// 1 sat/vB when it can estimate none, as on a fresh regtest); rates never
/// rise with the target. The node unreachable is an error, never a guess.
pub(super) async fn fee_estimates(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rpc = st.rpc.clone();
    on_node(move || {
        let calls: Vec<(&str, Value)> = FEE_TARGETS.iter().map(|t| ("estimatesmartfee", json!([t]))).collect();
        let answers = rpc.call_batch(&calls).map_err(ApiError::node)?;
        let rates: Vec<Option<f64>> = answers
            .into_iter()
            .map(|a| a.ok().and_then(|v| v["feerate"].as_f64()).map(|btc_per_kvb| btc_per_kvb * 1e5))
            .collect();
        Ok(Json(fee_map(&rates)))
    })
    .await
}

/// The `/fee-estimates` object from per-target rates (sat/vB), in
/// [`FEE_TARGETS`] order.
pub(crate) fn fee_map(rates: &[Option<f64>]) -> Value {
    let mut out = Map::new();
    let mut prev: Option<f64> = None;
    for (t, r) in FEE_TARGETS.iter().zip(rates) {
        let r = match (r.filter(|r| r.is_finite() && *r > 0.0), prev) {
            (Some(r), Some(p)) => r.min(p),
            (Some(r), None) => r,
            (None, Some(p)) => p,
            (None, None) => 1.0,
        };
        prev = Some(r);
        out.insert(t.to_string(), json!(r));
    }
    Value::Object(out)
}

// ---- blocks --------------------------------------------------------------

/// `GET /blocks/tip/hash` — the index's tip.
pub(super) async fn tip_hash(State(st): State<AppState>) -> ApiResult<Response> {
    let chain = st.chain.load();
    let (_, rec) = chain
        .tip()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "no blocks indexed yet"))?;
    Ok(text(StatusCode::OK, rec.hash.to_string()))
}

/// `GET /block-height/:height` — the index's main-chain block at `height`.
/// Heights below where the index starts (e.g. the genesis block, which BDK's
/// wallet sync always checks) come from the node: the index keeps only the
/// blocks it scanned.
pub(super) async fn block_height(State(st): State<AppState>, Path(height): Path<u32>) -> ApiResult<Response> {
    if let Some(rec) = st.chain.load().get(height) {
        return Ok(text(StatusCode::OK, rec.hash.to_string()));
    }
    let tip = st.chain.load().tip().map(|(h, _)| h);
    if tip.is_some_and(|t| height > t) {
        return Err(missing("block"));
    }
    let rpc = st.rpc.clone();
    on_node(move || {
        let v = call_opt(&rpc, "getblockhash", json!([height]))?.ok_or_else(|| missing("block"))?;
        let hash = v.as_str().ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "block hash is not a string"))?;
        Ok(text(StatusCode::OK, hash.to_string()))
    })
    .await
}

/// `GET /block/:hash/header` — the 80-byte header, hex.
pub(super) async fn block_header(State(st): State<AppState>, Path(hash): Path<String>) -> ApiResult<Response> {
    let hash = parse_hash(&hash)?;
    let rpc = st.rpc.clone();
    on_node(move || {
        let v = call_opt(&rpc, "getblockheader", json!([hash.to_string(), false]))?.ok_or_else(|| missing("block"))?;
        let hex = v.as_str().ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "header is not hex"))?;
        Ok(text(StatusCode::OK, hex.to_string()))
    })
    .await
}

/// `GET /block/:hash/status` — `{in_best_chain, height, next_best}`.
pub(super) async fn block_status(State(st): State<AppState>, Path(hash): Path<String>) -> ApiResult<Json<Value>> {
    let hash = parse_hash(&hash)?;
    let rpc = st.rpc.clone();
    on_node(move || {
        let v = call_opt(&rpc, "getblockheader", json!([hash.to_string(), true]))?;
        Ok(Json(block_status_json(v.as_ref())))
    })
    .await
}

/// Esplora's block status from a verbose `getblockheader` (`None`: unknown).
pub(crate) fn block_status_json(header: Option<&Value>) -> Value {
    match header {
        Some(h) if h["confirmations"].as_i64().unwrap_or(-1) >= 0 => json!({
            "in_best_chain": true,
            "height": h["height"],
            "next_best": h.get("nextblockhash").cloned().unwrap_or(Value::Null),
        }),
        _ => json!({ "in_best_chain": false }),
    }
}

fn block_cache() -> &'static Mutex<HashMap<BlockHash, Value>> {
    static CACHE: OnceLock<Mutex<HashMap<BlockHash, Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Esplora's block summary for `hash` from the node (cached by hash).
fn block_info(rpc: &Rpc, hash: &BlockHash) -> ApiResult<Value> {
    if let Some(v) = block_cache().lock().unwrap().get(hash) {
        return Ok(v.clone());
    }
    // Verbosity 1 carries size and weight; its txid list is dropped here.
    let b = call_opt(rpc, "getblock", json!([hash.to_string(), 1]))?.ok_or_else(|| missing("block"))?;
    let v = block_info_json(&b)?;
    let mut cache = block_cache().lock().unwrap();
    if cache.len() >= BLOCK_CACHE_MAX {
        cache.clear();
    }
    cache.insert(*hash, v.clone());
    Ok(v)
}

/// Esplora's `BlockInfo` from a verbose `getblock`.
pub(crate) fn block_info_json(b: &Value) -> ApiResult<Value> {
    let bits = b["bits"]
        .as_str()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "block without bits"))?;
    Ok(json!({
        "id": b["hash"],
        "height": b["height"],
        "version": b["version"],
        "timestamp": b["time"],
        "tx_count": b["nTx"],
        "size": b["size"],
        "weight": b["weight"],
        "merkle_root": b["merkleroot"],
        "previousblockhash": b.get("previousblockhash").cloned().unwrap_or(Value::Null),
        "mediantime": b["mediantime"],
        "nonce": b["nonce"],
        "bits": bits,
        "difficulty": b["difficulty"],
    }))
}

/// `GET /blocks` and `GET /blocks/:height` — the [`BLOCKS_PAGE`] blocks at
/// and below `height` (default the tip), newest first.
pub(super) async fn blocks_tip(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    blocks_from(st, None).await
}

pub(super) async fn blocks_at(State(st): State<AppState>, Path(height): Path<u32>) -> ApiResult<Json<Value>> {
    blocks_from(st, Some(height)).await
}

async fn blocks_from(st: AppState, height: Option<u32>) -> ApiResult<Json<Value>> {
    let chain = st.chain.load_full();
    let tip = chain.tip().map(|(h, _)| h).ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "no blocks indexed yet"))?;
    let top = height.unwrap_or(tip).min(tip);
    let hashes: Vec<BlockHash> = (0..BLOCKS_PAGE)
        .filter_map(|i| top.checked_sub(i))
        .filter_map(|h| chain.get(h).map(|r| r.hash))
        .collect();
    let rpc = st.rpc.clone();
    on_node(move || {
        let infos = hashes.iter().map(|h| block_info(&rpc, h)).collect::<ApiResult<Vec<_>>>()?;
        Ok(Json(Value::Array(infos)))
    })
    .await
}

// ---- transactions --------------------------------------------------------

/// `GET /tx/:txid/raw` — the transaction's bytes.
pub(super) async fn tx_raw(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Response> {
    let txid = parse_txid(&txid)?;
    on_node(move || {
        let hex = st.spends.node().raw_tx(&txid).map_err(ApiError::node)?.ok_or_else(|| missing("transaction"))?;
        let bytes = ::hex::decode(hex.trim()).map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
        Ok((StatusCode::OK, [(header::CONTENT_TYPE, "application/octet-stream")], Body::from(bytes)).into_response())
    })
    .await
}

/// A verbose transaction's Esplora status, from the node.
fn node_status(rpc: &Rpc, t: &Value) -> ApiResult<Value> {
    let Some(hash) = t["blockhash"].as_str() else {
        return Ok(json!({ "confirmed": false }));
    };
    let h = call_opt(rpc, "getblockheader", json!([hash, true]))?;
    Ok(match h {
        Some(h) if h["confirmations"].as_i64().unwrap_or(-1) >= 0 => json!({
            "confirmed": true,
            "block_height": h["height"],
            "block_hash": hash,
            "block_time": t["blocktime"],
        }),
        // Its block was reorganised away: back to unconfirmed.
        _ => json!({ "confirmed": false }),
    })
}

/// `getrawtransaction <txid> 2` (prevouts and fee included), `None` if unknown.
fn verbose_tx(rpc: &Rpc, txid: &Txid) -> ApiResult<Option<Value>> {
    call_opt(rpc, "getrawtransaction", json!([txid.to_string(), 2]))
}

/// `GET /tx/:txid/status`.
pub(super) async fn tx_status(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Json<Value>> {
    let txid = parse_txid(&txid)?;
    let rpc = st.rpc.clone();
    on_node(move || {
        let t = call_opt(&rpc, "getrawtransaction", json!([txid.to_string(), 1]))?.ok_or_else(|| missing("transaction"))?;
        Ok(Json(node_status(&rpc, &t)?))
    })
    .await
}

/// `GET /tx/:txid` — the full Esplora transaction (version, locktime, sizes,
/// inputs with scriptsig, witness and sequence), from the node: any
/// transaction, not only indexed addresses' (swaps read HTLCs from it, and
/// ldk-node its channel transactions).
pub(super) async fn tx_json(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Json<Value>> {
    let txid = parse_txid(&txid)?;
    on_node(move || {
        let node = st.spends.node();
        let t = node.tx(&txid).map_err(ApiError::node)?.ok_or_else(unknown_tx)?;
        let block = match t["blockhash"].as_str().map(str::parse::<BlockHash>) {
            Some(Ok(h)) => node.height_of(&h).map_err(ApiError::node)?.map(|n| (h, n)),
            _ => None,
        };
        let status = match block {
            Some((h, n)) => json!({
                "confirmed": true, "block_height": n, "block_hash": h.to_string(),
                "block_time": t["blocktime"],
            }),
            None => json!({ "confirmed": false }),
        };
        Ok(Json(full_esplora_tx(&t, status)))
    })
    .await
}

/// `GET /tx/:txid/merkleblock-proof` — BIP 37 merkle block (hex) proving the
/// transaction is in its block; 404 while unconfirmed.
pub(super) async fn merkleblock_proof(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Response> {
    let txid = parse_txid(&txid)?;
    let rpc = st.rpc.clone();
    on_node(move || {
        let v = call_opt(&rpc, "gettxoutproof", json!([[txid.to_string()]]))?.ok_or_else(|| missing("confirmed transaction"))?;
        let hex = v.as_str().ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "proof is not hex"))?;
        Ok(text(StatusCode::OK, hex.to_string()))
    })
    .await
}

/// The full Esplora shape of `getrawtransaction … 2`, `status` as given.
/// A superset of `render::esplora_tx`'s fields, so the wallet's own readers
/// see what they always did.
pub(crate) fn full_esplora_tx(t: &Value, status: Value) -> Value {
    let vin: Vec<Value> = t["vin"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| {
                    let coinbase = i.get("coinbase").is_some();
                    let prevout = match i.get("prevout") {
                        Some(p) if !p.is_null() => json!({
                            "scriptpubkey": p["scriptPubKey"]["hex"],
                            "scriptpubkey_address": p["scriptPubKey"]["address"],
                            "value": btc_to_sat(&p["value"]),
                        }),
                        _ => Value::Null,
                    };
                    json!({
                        "txid": if coinbase { json!("0".repeat(64)) } else { i["txid"].clone() },
                        "vout": if coinbase { json!(u32::MAX) } else { i["vout"].clone() },
                        "prevout": prevout,
                        // A coinbase's script is Core's `coinbase` field, not `scriptSig`.
                        "scriptsig": if coinbase {
                            i["coinbase"].as_str().unwrap_or("")
                        } else {
                            i["scriptSig"]["hex"].as_str().unwrap_or("")
                        },
                        "witness": i.get("txinwitness").cloned().unwrap_or_else(|| json!([])),
                        "sequence": i["sequence"],
                        "is_coinbase": coinbase,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let vout: Vec<Value> = t["vout"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|o| {
                    json!({
                        "scriptpubkey": o["scriptPubKey"]["hex"],
                        "scriptpubkey_address": o["scriptPubKey"]["address"],
                        "value": btc_to_sat(&o["value"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "txid": t["txid"],
        "version": t["version"],
        "locktime": t["locktime"],
        "vin": vin,
        "vout": vout,
        "size": t["size"],
        "weight": t["weight"],
        "fee": t.get("fee").map(btc_to_sat).unwrap_or(0),
        "status": status,
    })
}

// ---- script hash history -------------------------------------------------

fn parse_scripthash(s: &str) -> ApiResult<[u8; 32]> {
    let b = ::hex::decode(s).map_err(|_| ApiError::bad_request("bad script hash"))?;
    b.try_into().map_err(|_| ApiError::bad_request("bad script hash"))
}

/// `GET /scripthash/:sh/txs` — up to [`MEMPOOL_PAGE`] mempool transactions,
/// then the newest [`CHAIN_PAGE`] confirmed ones.
pub(super) async fn scripthash_txs(State(st): State<AppState>, Path(sh): Path<String>) -> ApiResult<Json<Value>> {
    scripthash_page(st, parse_scripthash(&sh)?, None).await
}

/// `GET /scripthash/:sh/txs/chain/:last_seen` — the next [`CHAIN_PAGE`]
/// confirmed transactions older than `last_seen`.
pub(super) async fn scripthash_txs_chain(
    State(st): State<AppState>,
    Path((sh, last)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let sh = parse_scripthash(&sh)?;
    let last = parse_txid(&last)?;
    scripthash_page(st, sh, Some(last)).await
}

/// Which of `rows` (newest first) a page shows: the first [`CHAIN_PAGE`], or
/// those after `last` (`None` when `last` isn't among them: nothing more).
pub(crate) fn page_after<T: Copy>(rows: &[(T, Txid)], last: Option<Txid>) -> Vec<T> {
    let start = match last {
        None => 0,
        Some(l) => match rows.iter().position(|(_, id)| *id == l) {
            Some(i) => i + 1,
            None => return Vec::new(),
        },
    };
    rows.iter().skip(start).take(CHAIN_PAGE).map(|(n, _)| *n).collect()
}

async fn scripthash_page(st: AppState, sh: [u8; 32], last: Option<Txid>) -> ApiResult<Json<Value>> {
    // Txs to render: (pending txids) and (confirmed txnum, height, block) rows.
    let (pending, confirmed) = with_view(&st, move |_, v, mp| {
        // Indexed blocks first; else a pending payment is the script's first.
        let found = v.reader.program_for_scripthash(&sh).map_err(ApiError::index)?;
        let Some(p) = found.or_else(|| mp.program_for_scripthash(&sh)) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let pending: Vec<Txid> = if last.is_none() {
            let ids = mp.txids_for(&p);
            ids.into_iter().take(MEMPOOL_PAGE.min(PENDING_MAX)).collect()
        } else {
            Vec::new()
        };
        let rows = v.reader.history(&p, MAX_SCRIPT_HISTORY).map_err(ApiError::index)?;
        let mut with_ids: Vec<(TxNum, Txid)> = Vec::with_capacity(rows.len());
        for n in rows {
            if let Some(id) = v.reader.txid(n).map_err(ApiError::index)? {
                // A tx the mempool snapshot still lists is shown once, as pending.
                if !pending.contains(&id) {
                    with_ids.push((n, id));
                }
            }
        }
        let page = page_after(&with_ids, last);
        let confirmed: Vec<(Txid, Value)> = page
            .into_iter()
            .filter_map(|n| {
                let id = with_ids.iter().find(|(m, _)| *m == n)?.1;
                let h = v.chain.height_of(n)?;
                let rec = v.chain.get(h)?;
                Some((
                    id,
                    json!({
                        "confirmed": true,
                        "block_height": h,
                        "block_hash": rec.hash.to_string(),
                        "block_time": rec.time,
                    }),
                ))
            })
            .collect();
        Ok((pending, confirmed))
    })
    .await?;
    let rpc = st.rpc.clone();
    on_node(move || {
        let mut out = Vec::with_capacity(pending.len() + confirmed.len());
        for id in pending {
            // Mined or replaced since the snapshot: skip, the next sync sees it.
            if let Some(t) = verbose_tx(&rpc, &id)? {
                if t.get("blockhash").is_none() {
                    out.push(full_esplora_tx(&t, json!({ "confirmed": false })));
                }
            }
        }
        for (id, status) in confirmed {
            let t = verbose_tx(&rpc, &id)?.ok_or_else(|| {
                ApiError::new(StatusCode::BAD_GATEWAY, format!("the node does not know indexed transaction {id}"))
            })?;
            out.push(full_esplora_tx(&t, status));
        }
        Ok(Json(Value::Array(out)))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use esplora_client::api::{BlockInfo, BlockStatus, OutputStatus, Tx, TxStatus};

    /// `getrawtransaction … 2` for a segwit spend, as Bitcoin Core answers it.
    fn core_tx() -> Value {
        json!({
            "txid": "f177ab8d6bdcaa876483f0ebf4528d4c023a4785cfb0b29734b786ce7cbba5d9",
            "hash": "aa77ab8d6bdcaa876483f0ebf4528d4c023a4785cfb0b29734b786ce7cbba5d9",
            "version": 2, "size": 222, "vsize": 141, "weight": 561, "locktime": 974_000,
            "vin": [{
                "txid": "0af166a14c972ad1f2e0cb06953e7e63d67ce786d2142f2bca8823248f79990f",
                "vout": 1,
                "scriptSig": { "asm": "", "hex": "" },
                "txinwitness": ["3044022010", "02aabbcc"],
                "prevout": {
                    "generated": false, "height": 973_999, "value": 0.00600000,
                    "scriptPubKey": { "hex": "0014751e76e8199196d454941c45d1b3a323f1433bd6",
                                      "address": "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "type": "witness_v0_keyhash" }
                },
                "sequence": 4_294_967_293u64
            }],
            "vout": [{
                "value": 0.00526031, "n": 0,
                "scriptPubKey": { "hex": "0014751e76e8199196d454941c45d1b3a323f1433bd6",
                                  "address": "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", "type": "witness_v0_keyhash" }
            }],
            "fee": 0.00001143,
            "blockhash": "00000000000000000001b8e5fa1a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e",
            "blocktime": 1_762_180_000u64
        })
    }

    fn coinbase_tx() -> Value {
        json!({
            "txid": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
            "version": 1, "size": 204, "weight": 816, "locktime": 0,
            "vin": [{ "coinbase": "04ffff001d0104", "sequence": 4_294_967_295u64 }],
            "vout": [{ "value": 50.0, "n": 0, "scriptPubKey": { "hex": "51" } }]
        })
    }

    #[test]
    fn full_tx_parses_as_ldk_nodes_esplora_client_reads_it() {
        let status = json!({ "confirmed": true, "block_height": 974_000, "block_hash":
            "00000000000000000001b8e5fa1a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e", "block_time": 1_762_180_000u64 });
        let v = full_esplora_tx(&core_tx(), status);
        let tx: Tx = serde_json::from_value(v.clone()).expect("esplora-client parses it");
        assert_eq!(tx.version, 2);
        assert_eq!(tx.locktime, 974_000);
        assert_eq!((tx.size, tx.weight, tx.fee), (222, 561, 1_143));
        assert_eq!(tx.vin[0].vout, 1);
        assert_eq!(tx.vin[0].sequence, 4_294_967_293);
        assert_eq!(tx.vin[0].witness, vec![vec![0x30, 0x44, 0x02, 0x20, 0x10], vec![0x02, 0xaa, 0xbb, 0xcc]]);
        assert!(!tx.vin[0].is_coinbase);
        assert_eq!(tx.vin[0].prevout.as_ref().unwrap().value, 600_000);
        assert_eq!(tx.vout[0].value, 526_031);
        assert!(tx.status.confirmed);
        assert_eq!(tx.status.block_height, Some(974_000));
        // The fields the wallet's own reader uses are still there.
        assert_eq!(v["vin"][0]["prevout"]["scriptpubkey_address"], "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
        assert_eq!(v["vout"][0]["scriptpubkey_address"], "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4");
    }

    #[test]
    fn a_coinbase_and_an_unconfirmed_tx_parse_too() {
        let tx: Tx = serde_json::from_value(full_esplora_tx(&coinbase_tx(), json!({ "confirmed": false }))).unwrap();
        assert!(tx.vin[0].is_coinbase);
        assert_eq!(tx.vin[0].scriptsig.as_bytes(), [0x04, 0xff, 0xff, 0x00, 0x1d, 0x01, 0x04]);
        assert!(tx.vin[0].prevout.is_none());
        assert!(tx.vin[0].witness.is_empty());
        assert_eq!(tx.fee, 0);
        assert!(!tx.status.confirmed);
        let _: TxStatus = serde_json::from_value(json!({ "confirmed": false })).unwrap();
    }

    #[test]
    fn fee_estimates_parse_fill_gaps_and_never_rise() {
        let v = fee_map(&[Some(20.0), None, Some(25.0), Some(10.0), None, Some(5.0), None, None, Some(2.0), None, None]);
        let m: HashMap<u16, f64> = serde_json::from_value(v).expect("esplora-client's shape");
        assert_eq!(m.len(), FEE_TARGETS.len());
        assert_eq!(m[&1], 20.0);
        assert_eq!(m[&2], 20.0, "a gap takes the faster target's rate");
        assert_eq!(m[&3], 20.0, "never higher for a slower target");
        assert_eq!(m[&4], 10.0);
        assert_eq!(m[&6], 5.0);
        assert_eq!(m[&1008], 2.0);
        let none: HashMap<u16, f64> = serde_json::from_value(fee_map(&[None; 11])).unwrap();
        assert!(none.values().all(|r| *r == 1.0), "regtest: 1 sat/vB everywhere");
        // Nonsense from the node is ignored, not passed on.
        let odd: HashMap<u16, f64> = serde_json::from_value(fee_map(&[Some(-1.0), Some(f64::NAN), Some(3.0), None, None, None, None, None, None, None, None])).unwrap();
        assert_eq!(odd[&1], 1.0);
        assert_eq!(odd[&3], 1.0);
    }

    #[test]
    fn block_info_and_status_parse() {
        let b = json!({
            "hash": "00000000000000000001b8e5fa1a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e", "height": 974_000,
            "version": 536_870_912, "time": 1_762_180_000u64, "mediantime": 1_762_179_000u64, "nonce": 12_345,
            "bits": "17023a04", "difficulty": 1.5e14, "nTx": 3_000, "size": 1_500_000, "weight": 3_990_000,
            "merkleroot": "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b",
            "previousblockhash": "000000000000000000000000001a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e",
            "tx": ["…"]
        });
        let info: BlockInfo = serde_json::from_value(block_info_json(&b).unwrap()).expect("BlockInfo");
        assert_eq!(info.height, 974_000);
        assert_eq!(info.bits.to_consensus(), 0x1702_3a04);
        assert_eq!((info.tx_count, info.size, info.weight), (3_000, 1_500_000, 3_990_000));
        assert!(info.previousblockhash.is_some());
        let mut genesis = b.clone();
        genesis.as_object_mut().unwrap().remove("previousblockhash");
        let g: BlockInfo = serde_json::from_value(block_info_json(&genesis).unwrap()).unwrap();
        assert!(g.previousblockhash.is_none());

        let main: BlockStatus = serde_json::from_value(block_status_json(Some(&json!({
            "confirmations": 3, "height": 974_000,
            "nextblockhash": "000000000000000000000000001a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e"
        })))).unwrap();
        assert!(main.in_best_chain && main.height == Some(974_000) && main.next_best.is_some());
        let tip: BlockStatus = serde_json::from_value(block_status_json(Some(&json!({ "confirmations": 1, "height": 5 })))).unwrap();
        assert!(tip.in_best_chain && tip.next_best.is_none());
        let stale: BlockStatus = serde_json::from_value(block_status_json(Some(&json!({ "confirmations": -1, "height": 5 })))).unwrap();
        assert!(!stale.in_best_chain);
        let unknown: BlockStatus = serde_json::from_value(block_status_json(None)).unwrap();
        assert!(!unknown.in_best_chain);
    }

    #[test]
    fn outspend_json_parses_as_output_status() {
        use crate::spends::Outspend;
        let id: Txid = "f177ab8d6bdcaa876483f0ebf4528d4c023a4785cfb0b29734b786ce7cbba5d9".parse().unwrap();
        let h: BlockHash = "00000000000000000001b8e5fa1a5e7d5c3d0d6f0e2e8f3c5f8e9d1a2b3c4d5e".parse().unwrap();
        for o in [Outspend::Unspent, Outspend::Spent { txid: id, vin: 0, block: None }, Outspend::Spent { txid: id, vin: 2, block: Some((h, 7)) }] {
            let s: OutputStatus = serde_json::from_value(o.json()).expect("OutputStatus");
            assert_eq!(s.spent, !matches!(o, Outspend::Unspent));
        }
    }

    #[test]
    fn script_hash_is_the_one_esplora_client_asks_for() {
        use bitcoin::hashes::sha256;
        let program = [0x75u8; 20];
        let spk = bitcoin::ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array(program));
        // esplora-client: `sha256::Hash::hash(script.as_bytes())`, formatted `{:x}`.
        let theirs = format!("{:x}", sha256::Hash::hash(spk.as_bytes()));
        let ours = ::hex::encode(crate::keys::p2wpkh_scripthash(&program));
        assert_eq!(ours, theirs);
        assert_eq!(parse_scripthash(&theirs).unwrap(), crate::keys::p2wpkh_scripthash(&program));
        assert!(parse_scripthash("abcd").is_err());
    }

    #[test]
    fn pages_follow_last_seen() {
        let id = |n: u8| Txid::from_byte_array([n; 32]);
        let rows: Vec<(u64, Txid)> = (0..60u8).map(|n| (n as u64, id(n))).collect();
        assert_eq!(page_after(&rows, None), (0..25).collect::<Vec<u64>>());
        assert_eq!(page_after(&rows, Some(id(24))), (25..50).collect::<Vec<u64>>());
        assert_eq!(page_after(&rows, Some(id(49))), (50..60).collect::<Vec<u64>>());
        assert!(page_after(&rows, Some(id(59))).is_empty());
        assert!(page_after(&rows, Some(id(200))).is_empty(), "an unknown last_seen ends the walk");
    }
}
