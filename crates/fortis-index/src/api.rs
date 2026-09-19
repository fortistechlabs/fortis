//! The Esplora-shaped REST surface the fortis wallet's `EsploraBackend` calls:
//! `/blocks/tip/height`, `/address/:a/utxo`, `/address/:a/txs`,
//! `/v1/fees/recommended`, and `POST /tx` — plus `POST /scan`, which answers a
//! whole wallet's balance + history for many addresses in one call (see
//! [`scan_route`]). Public chain data only — no auth; bind to localhost or a
//! trusted network, or front it with a TLS/rate-limiting proxy.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{anyhow, Context, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, ScriptBuf};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use fortis_node::Rpc;

use crate::mempool::Mempool;
use crate::store::{HistTx, Store, SPK_LEN};

enum Reply {
    Json(u16, Value),
    Text(u16, String),
    Empty(u16),
}

pub fn serve(
    bind: &str,
    store: Store,
    rpc: Arc<Rpc>,
    mempool: Arc<RwLock<Mempool>>,
    network: Network,
) -> Result<()> {
    let server = Server::http(bind).map_err(|e| anyhow!("cannot bind {bind}: {e}"))?;
    eprintln!("fortis-index listening on  http://{bind}");
    let tx_cache = TxCache::default();
    for mut req in server.incoming_requests() {
        let body = read_body(&mut req);
        let mp = mempool.read().unwrap();
        let reply = route(&req, &store, &rpc, &mp, network, &body, &tx_cache);
        drop(mp);
        let _ = respond(req, reply);
    }
    Ok(())
}

/// Mined-transaction detail (Esplora-shaped), keyed by `(txid, block hash)`.
///
/// The node RPC that produces it (`getrawtransaction … 2`) costs ~12 ms per
/// transaction and the answer never changes once mined, yet every wallet
/// refresh used to pay it again for the same ~50 transactions — that alone was
/// ~600 ms of a `/scan` and nearly all of a cold `/txs`. The block hash is part
/// of the key so a reorg that re-mines a tx in a different block misses
/// instead of serving the old block's height. Bounded by clearing when full:
/// one wallet's working set is ~50 entries, so this only ever thrashes past a
/// couple of hundred concurrently-active wallets, and a miss is merely slow.
#[derive(Default)]
struct TxCache(Mutex<HashMap<(String, String), Value>>);

const TX_CACHE_MAX: usize = 20_000;
/// Concurrent node RPCs for a batch of cache misses — comfortably under the
/// node's default RPC work queue so the sync thread's own calls are never starved.
const RPC_PARALLEL: usize = 8;

impl TxCache {
    fn get(&self, key: &(String, String)) -> Option<Value> {
        self.0.lock().unwrap().get(key).cloned()
    }

    fn insert(&self, key: (String, String), tx: Value) {
        let mut m = self.0.lock().unwrap();
        if m.len() >= TX_CACHE_MAX {
            m.clear();
        }
        m.insert(key, tx);
    }
}

/// The Esplora tx for every row, in order: from `cache` when present, else via
/// `fetch` — misses run up to [`RPC_PARALLEL`] at a time and are cached.
fn confirmed_txs<F>(rows: &[HistTx], cache: &TxCache, fetch: F) -> Result<Vec<Value>>
where
    F: Fn(&HistTx) -> Result<Value> + Sync,
{
    let key = |h: &HistTx| (h.txid.clone(), h.block_hash.clone());
    let mut out: Vec<Option<Value>> = rows.iter().map(|h| cache.get(&key(h))).collect();
    let misses: Vec<usize> = (0..rows.len()).filter(|&i| out[i].is_none()).collect();
    for group in misses.chunks(RPC_PARALLEL) {
        let fetched: Vec<Result<Value>> = std::thread::scope(|s| {
            let handles: Vec<_> = group
                .iter()
                .map(|&i| {
                    let fetch = &fetch;
                    s.spawn(move || fetch(&rows[i]))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow!("node RPC worker panicked"))))
                .collect()
        });
        for (&i, r) in group.iter().zip(fetched) {
            let tx = r?;
            cache.insert(key(&rows[i]), tx.clone());
            out[i] = Some(tx);
        }
    }
    Ok(out.into_iter().map(|v| v.expect("every row filled or an error returned")).collect())
}

/// One mined transaction's Esplora shape, straight from the node.
fn fetch_confirmed(rpc: &Rpc, h: &HistTx) -> Result<Value> {
    let t = rpc
        .call("getrawtransaction", json!([h.txid, 2, h.block_hash]))
        .with_context(|| format!("getrawtransaction {}", h.txid))?;
    Ok(esplora_tx(&t, Some(h.height)))
}

fn route(
    req: &Request,
    store: &Store,
    rpc: &Rpc,
    mp: &Mempool,
    network: Network,
    body: &str,
    tx_cache: &TxCache,
) -> Reply {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").trim_end_matches('/');

    if method == Method::Options {
        return Reply::Empty(204);
    }
    match (&method, path) {
        (Method::Get, "") | (Method::Get, "/") => Reply::Json(
            200,
            json!({
                "name": "fortis-index",
                "version": env!("CARGO_PKG_VERSION"),
                "tip": store.tip().ok().flatten().map(|(h, _)| h),
                "mempool": mp.len(),
            }),
        ),
        (Method::Get, "/blocks/tip/height") => match store.tip() {
            Ok(Some((h, _))) => Reply::Text(200, h.to_string()),
            Ok(None) => Reply::Text(200, "0".into()),
            Err(e) => err(500, e),
        },
        (Method::Get, "/v1/fees/recommended") => Reply::Json(200, recommended_fees(rpc)),
        (Method::Post, "/tx") => match fortis_node::broadcast(rpc, body.trim()) {
            Ok(txid) => Reply::Text(200, txid.to_string()),
            Err(e) => Reply::Text(400, format!("{e:#}")),
        },
        (Method::Post, "/scan") => scan_route(body, store, rpc, mp, network, tx_cache),
        (Method::Get, p) if p.starts_with("/address/") => {
            address_route(p, store, rpc, mp, network, tx_cache)
        }
        _ => err(404, "no such route"),
    }
}

fn address_route(
    path: &str,
    store: &Store,
    rpc: &Rpc,
    mp: &Mempool,
    network: Network,
    tx_cache: &TxCache,
) -> Reply {
    // /address/<addr>/utxo  or  /address/<addr>/txs
    let rest = &path["/address/".len()..];
    let (addr, tail) = match rest.split_once('/') {
        Some(x) => x,
        None => return err(404, "expected /address/<addr>/utxo or /txs"),
    };
    let spk = match address_spk(addr, network) {
        Ok(s) => s,
        Err(e) => return err(400, e),
    };

    match tail {
        "utxo" => match address_utxo(&spk, store, mp) {
            Ok(v) => Reply::Json(200, v),
            Err(e) => err(500, e),
        },
        "txs" => match address_txs(&spk, store, rpc, mp, network, tx_cache) {
            Ok(v) => Reply::Json(200, v),
            Err(e) => err(502, e),
        },
        _ => err(404, "expected /utxo or /txs"),
    }
}

/// Confirmed UTXOs (minus any spent by a pending tx) followed by unconfirmed
/// ones the mempool creates for this address.
fn address_utxo(spk: &str, store: &Store, mp: &Mempool) -> Result<Value> {
    let mut rows: Vec<Value> = store
        .utxos_for(spk)?
        .into_iter()
        .filter(|u| !mp.is_spent(&u.txid, u.vout))
        .map(|u| {
            json!({
                "txid": u.txid,
                "vout": u.vout,
                "value": u.value_sat,
                "status": { "confirmed": true, "block_height": u.height },
            })
        })
        .collect();
    for (txid, vout, value) in mp.utxos_for(spk) {
        rows.push(json!({
            "txid": txid,
            "vout": vout,
            "value": value,
            "status": { "confirmed": false },
        }));
    }
    Ok(Value::Array(rows))
}

/// An address's transactions in the Esplora shape the client parses
/// (`vin[].prevout.{scriptpubkey_address,value}`, `vout[].{scriptpubkey_address,
/// value}`, `fee`, `status`). Mempool txs first, then confirmed newest-first. The
/// index stores only txid lists; confirmed detail comes from `getrawtransaction
/// <txid> 2 <blockhash>` (no txindex needed), mempool detail from the overlay.
fn address_txs(
    spk: &str,
    store: &Store,
    rpc: &Rpc,
    mp: &Mempool,
    network: Network,
    tx_cache: &TxCache,
) -> Result<Value> {
    let mut txs: Vec<Value> = mp
        .txs_for(spk)
        .into_iter()
        .map(|t| esplora_tx(&backfill_prevouts(t, store, mp, network), None))
        .collect();
    let pending: std::collections::HashSet<String> =
        txs.iter().filter_map(|t| t["txid"].as_str().map(str::to_string)).collect();

    // A block may just have landed that the mempool snapshot still lists.
    let rows: Vec<HistTx> =
        store.history_for(spk, 100)?.into_iter().filter(|h| !pending.contains(&h.txid)).collect();
    txs.extend(confirmed_txs(&rows, tx_cache, |h| fetch_confirmed(rpc, h))?);
    Ok(Value::Array(txs))
}

/// Most addresses one `POST /scan` may name. This server answers one request at
/// a time, so this bounds how long a single call can hold every other one up.
const SCAN_MAX_ADDRESSES: usize = 1000;
const SCAN_DEFAULT_HISTORY: usize = 50;
const SCAN_MAX_HISTORY: usize = 100;

/// `POST /scan` — body `{"addresses": [...], "history": 50}`. Everything a
/// wallet needs to show its balance and recent activity for a whole address
/// set at once, instead of two round trips per address:
///
/// ```text
/// { "tip":    972801,
///   "used":   ["bc1q…", …],            // appeared in any tx, pending included
///   "utxos":  [{ "address", "txid", "vout", "value", "status" }, …],
///   "txs":    [ <Esplora tx>, … ],     // pending first, then newest confirmed; each tx once
///   "failed": [] }                     // addresses that couldn't be checked
/// ```
///
/// `failed` is always empty here (one local database — the answer is all or
/// nothing); it exists so every backend that serves `/scan` (the edge's BTC
/// path can partially fail) speaks one shape, and a client can refuse to show
/// a balance built from an incomplete scan.
fn scan_route(
    body: &str,
    store: &Store,
    rpc: &Rpc,
    mp: &Mempool,
    network: Network,
    tx_cache: &TxCache,
) -> Reply {
    let req: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return err(400, format!("body must be JSON: {e}")),
    };
    let Some(list) = req["addresses"].as_array() else {
        return err(400, "expected {\"addresses\": [...]}");
    };
    if list.is_empty() || list.len() > SCAN_MAX_ADDRESSES {
        return err(400, format!("addresses: expected 1 to {SCAN_MAX_ADDRESSES}"));
    }
    let history =
        req["history"].as_u64().map_or(SCAN_DEFAULT_HISTORY, |n| n as usize).min(SCAN_MAX_HISTORY);

    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(list.len());
    for v in list {
        let Some(addr) = v.as_str() else { return err(400, "addresses must be strings") };
        if !seen.insert(addr) {
            continue;
        }
        match address_spk(addr, network) {
            // The index only stores P2WPKH outputs — anything else would look
            // "unused" here, which is a wrong answer, not an empty one.
            Ok(spk) if spk.len() == SPK_LEN * 2 => targets.push((addr.to_string(), spk)),
            Ok(_) => return err(400, format!("address {addr}: only P2WPKH addresses are indexed")),
            Err(e) => return err(400, e),
        }
    }

    let plan = match scan_plan(&targets, history, store, mp, network) {
        Ok(p) => p,
        Err(e) => return err(500, e),
    };
    match scan_finish(plan, store, rpc, tx_cache) {
        Ok(v) => Reply::Json(200, v),
        Err(e) => err(502, e),
    }
}

/// Everything `POST /scan` can answer from the index and mempool alone — no
/// node RPC. Split from [`scan_finish`] so it's testable without a node.
struct ScanPlan {
    used: Vec<String>,
    utxos: Vec<Value>,
    /// Mempool txs, already Esplora-shaped.
    pending: Vec<Value>,
    /// Newest-first, each txid once, already capped to the requested history.
    confirmed: Vec<HistTx>,
}

fn scan_plan(
    targets: &[(String, String)],
    history: usize,
    store: &Store,
    mp: &Mempool,
    network: Network,
) -> Result<ScanPlan> {
    let mut used = Vec::new();
    let mut utxos = Vec::new();
    let mut pending = Vec::new();
    let mut pending_ids: HashSet<String> = HashSet::new();
    let mut confirmed: Vec<HistTx> = Vec::new();
    let mut confirmed_ids: HashSet<String> = HashSet::new();

    for (addr, spk) in targets {
        let rows = store.history_for(spk, history)?;
        let mem = mp.txs_for(spk);
        if !rows.is_empty() || !mem.is_empty() {
            used.push(addr.clone());
        }

        if let Value::Array(unspent) = address_utxo(spk, store, mp)? {
            for mut u in unspent {
                u["address"] = json!(addr);
                utxos.push(u);
            }
        }
        for t in mem {
            let id = t["txid"].as_str().unwrap_or_default().to_string();
            if pending_ids.insert(id) {
                pending.push(esplora_tx(&backfill_prevouts(t, store, mp, network), None));
            }
        }
        for h in rows {
            if confirmed_ids.insert(h.txid.clone()) {
                confirmed.push(h);
            }
        }
    }

    // A block may just have landed that the mempool snapshot still lists.
    confirmed.retain(|h| !pending_ids.contains(&h.txid));
    // The newest `history` overall are within the newest `history` of each
    // address, so the per-address cap above lost nothing.
    confirmed.sort_by(|a, b| b.height.cmp(&a.height).then_with(|| a.txid.cmp(&b.txid)));
    confirmed.truncate(history);
    Ok(ScanPlan { used, utxos, pending, confirmed })
}

/// Fetch full detail for the (at most `history`) confirmed txs the plan kept —
/// the only per-tx node RPC a scan pays (instead of one per tx per address),
/// and none at all for a transaction already seen (see [`TxCache`]).
fn scan_finish(plan: ScanPlan, store: &Store, rpc: &Rpc, tx_cache: &TxCache) -> Result<Value> {
    let tip = store.tip()?.map_or(0, |(h, _)| h);
    let mut txs = plan.pending;
    txs.extend(confirmed_txs(&plan.confirmed, tx_cache, |h| fetch_confirmed(rpc, h))?);
    Ok(json!({
        "tip": tip,
        "used": plan.used,
        "utxos": plan.utxos,
        "txs": txs,
        "failed": Vec::<String>::new(),
    }))
}

/// Some Knots/BLAKE2b nodes omit `vin[].prevout` for a *mempool* transaction's
/// inputs (`getrawtransaction <txid> 2` only fills it once the tx is mined).
/// Without it the client can't tell that a pending tx spends the wallet's own
/// coins, so an outgoing payment shows as an incoming receive of its change.
/// Fill any missing prevout from the confirmed index / the mempool overlay.
/// A no-op when the node already provided the prevout.
fn backfill_prevouts(t: &Value, store: &Store, mp: &Mempool, network: Network) -> Value {
    let mut t = t.clone();
    let Some(vins) = t.get_mut("vin").and_then(Value::as_array_mut) else { return t };
    for i in vins {
        if i.get("prevout").is_some_and(|p| !p.is_null()) {
            continue;
        }
        let (Some(ptxid), Some(pvout)) = (i["txid"].as_str(), i["vout"].as_u64()) else { continue };
        let (ptxid, pvout) = (ptxid.to_string(), pvout as u32);
        let Some((spk_hex, value_sat)) = mp
            .output_at(&ptxid, pvout)
            .or_else(|| store.output_at(&ptxid, pvout).ok().flatten())
        else {
            continue;
        };
        let Some(addr) = spk_hex_to_address(&spk_hex, network) else { continue };
        i["prevout"] = json!({
            "scriptPubKey": { "address": addr },
            "value": value_sat as f64 / 1e8,
        });
    }
    t
}

fn spk_hex_to_address(spk_hex: &str, network: Network) -> Option<String> {
    let spk = ScriptBuf::from(hex::decode(spk_hex).ok()?);
    Address::from_script(&spk, network).ok().map(|a| a.to_string())
}

/// `confirmed_at` is `Some(height)` for a mined tx, `None` for a mempool one.
fn esplora_tx(t: &Value, confirmed_at: Option<u64>) -> Value {
    let vin: Vec<Value> = t["vin"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| match i.get("prevout") {
                    Some(p) if !p.is_null() => json!({
                        "prevout": {
                            "scriptpubkey_address": p["scriptPubKey"]["address"],
                            "value": btc_to_sat(&p["value"]),
                        }
                    }),
                    _ => json!({ "prevout": Value::Null }),
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
                        "scriptpubkey_address": o["scriptPubKey"]["address"],
                        "value": btc_to_sat(&o["value"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let status = match confirmed_at {
        Some(height) => json!({
            "confirmed": true,
            "block_height": height,
            "block_time": t.get("blocktime").and_then(Value::as_u64)
                .or_else(|| t.get("time").and_then(Value::as_u64)),
        }),
        None => json!({ "confirmed": false }),
    };
    json!({
        "txid": t["txid"],
        "vin": vin,
        "vout": vout,
        "fee": t.get("fee").map(btc_to_sat).unwrap_or(0),
        "status": status,
    })
}

fn btc_to_sat(v: &Value) -> u64 {
    (v.as_f64().unwrap_or(0.0) * 1e8).round().max(0.0) as u64
}

fn recommended_fees(rpc: &Rpc) -> Value {
    let at = |t: u16| fortis_node::estimate_feerate(rpc, t).unwrap_or(1);
    let fastest = at(1);
    let half = at(3).min(fastest);
    let hour = at(6).min(half);
    let economy = at(144).min(hour);
    json!({
        "fastestFee": fastest,
        "halfHourFee": half,
        "hourFee": hour,
        "economyFee": economy,
        "minimumFee": 1u64,
    })
}

fn address_spk(addr: &str, network: Network) -> Result<String> {
    let a = addr
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|e| anyhow!("bad address {addr}: {e}"))?
        .require_network(network)
        .map_err(|_| anyhow!("address {addr} is not valid on this network"))?;
    Ok(hex::encode(a.script_pubkey().as_bytes()))
}

fn err(status: u16, msg: impl std::fmt::Display) -> Reply {
    Reply::Json(status, json!({ "error": msg.to_string() }))
}

fn read_body(req: &mut Request) -> String {
    if req.method() == &Method::Post {
        // Only `POST /tx` has a body — a consensus-max tx is ~2 MB of hex. Cap the
        // read so a bogus Content-Length can't exhaust memory.
        let mut s = String::new();
        let _ = req.as_reader().take(2 * 1024 * 1024).read_to_string(&mut s);
        s
    } else {
        String::new()
    }
}

fn respond(req: Request, reply: Reply) -> std::io::Result<()> {
    let (status, ctype, data): (u16, &str, Vec<u8>) = match reply {
        Reply::Empty(s) => (s, "text/plain", Vec::new()),
        Reply::Text(s, t) => (s, "text/plain", t.into_bytes()),
        Reply::Json(s, v) => (s, "application/json", serde_json::to_vec(&v).unwrap_or_default()),
    };
    let mut resp = Response::from_data(data).with_status_code(status);
    for (k, v) in [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
        ("Access-Control-Allow-Headers", "content-type"),
        ("Content-Type", ctype),
    ] {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    req.respond(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{IndexedTx, Store, TxIn, TxOut};

    // BIP-173 P2WPKH example program.
    const P2WPKH_SPK: &str = "0014751e76e8199196d454941c45d1b3a323f1433bd6";
    const P2WPKH_ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    static NEXT_TEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn test_store_path() -> String {
        std::env::temp_dir()
            .join(format!(
                "fortis-index-api-test-{}-{}",
                std::process::id(),
                NEXT_TEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))
            .to_str()
            .unwrap()
            .to_string()
    }

    // Padded out to a real txid's length (32 bytes / 64 hex chars) -- still
    // valid hex since the seed itself only uses hex digits.
    fn txid(seed: &str) -> String {
        format!("{seed:0>64}")
    }

    #[test]
    fn spk_hex_to_address_round_trips_a_p2wpkh() {
        assert_eq!(spk_hex_to_address(P2WPKH_SPK, Network::Bitcoin).as_deref(), Some(P2WPKH_ADDR));
        assert_eq!(spk_hex_to_address("not-hex", Network::Bitcoin), None);
        assert_eq!(spk_hex_to_address("00", Network::Bitcoin), None); // not a known template
    }

    #[test]
    fn backfill_fills_a_null_mempool_prevout_from_the_confirmed_index() {
        let mut store = Store::open(&test_store_path()).unwrap();
        store
            .apply_block(
                100,
                &txid("100"),
                &[IndexedTx {
                    txid: txid("aa"),
                    inputs: vec![],
                    outputs: vec![TxOut { vout: 0, spk_hex: P2WPKH_SPK.into(), value_sat: 500_000 }],
                }],
            )
            .unwrap();
        let mp = Mempool::default();

        // a mempool tx spending aa:0 that the node reported with no prevout
        let pending = json!({
            "txid": txid("bb"),
            "vin": [{ "txid": txid("aa"), "vout": 0 }],
            "vout": [{ "value": 0.004, "scriptPubKey": { "address": "bc1qdest" } }],
        });
        let filled = backfill_prevouts(&pending, &store, &mp, Network::Bitcoin);

        let e = esplora_tx(&filled, None);
        assert_eq!(e["vin"][0]["prevout"]["scriptpubkey_address"], P2WPKH_ADDR);
        assert_eq!(e["vin"][0]["prevout"]["value"], 500_000);
        assert_eq!(e["status"]["confirmed"], false);
    }

    #[test]
    fn backfill_leaves_a_prevout_the_node_already_gave_us_untouched() {
        let store = Store::open(&test_store_path()).unwrap();
        let mp = Mempool::default();
        let tx = json!({
            "txid": txid("bb"),
            "vin": [{
                "txid": txid("aa"), "vout": 0,
                "prevout": { "scriptPubKey": { "address": "bc1qkeep" }, "value": 0.001 }
            }],
            "vout": [],
        });
        let filled = backfill_prevouts(&tx, &store, &mp, Network::Bitcoin);
        assert_eq!(filled["vin"][0]["prevout"]["scriptPubKey"]["address"], "bc1qkeep");
    }

    /// A P2WPKH spk with a recognisable 20-byte program, and its address.
    fn p2wpkh(seed: &str) -> (String, String) {
        let spk = format!("0014{seed:0>40}");
        let addr = spk_hex_to_address(&spk, Network::Bitcoin).unwrap();
        (spk, addr)
    }

    fn out(vout: u32, spk: &str, value_sat: u64) -> TxOut {
        TxOut { vout, spk_hex: spk.into(), value_sat }
    }

    /// Three blocks over two of the wallet's addresses (`a`, `b`) plus one
    /// it doesn't own (`z`), and a third wallet address (`n`) never used:
    ///   100  aa   pays a:500, b:300
    ///   101  bb   pays a:200 and b:100 in ONE tx (a tx touching two of our addresses)
    ///   102  cc   spends aa:0 (a's 500) to z:480
    fn scan_fixture() -> (Store, Vec<(String, String)>) {
        let mut s = Store::open(&test_store_path()).unwrap();
        let (a, b, z) = (p2wpkh("a1").0, p2wpkh("b1").0, p2wpkh("f1").0);
        s.apply_block(100, &txid("100"), &[IndexedTx {
            txid: txid("aa"), inputs: vec![], outputs: vec![out(0, &a, 500), out(1, &b, 300)],
        }]).unwrap();
        s.apply_block(101, &txid("101"), &[IndexedTx {
            txid: txid("bb"), inputs: vec![], outputs: vec![out(0, &a, 200), out(1, &b, 100)],
        }]).unwrap();
        s.apply_block(102, &txid("102"), &[IndexedTx {
            txid: txid("cc"), inputs: vec![TxIn { txid: txid("aa"), vout: 0 }], outputs: vec![out(0, &z, 480)],
        }]).unwrap();
        let targets = ["a1", "b1", "d1"].map(|seed| {
            let (spk, addr) = p2wpkh(seed);
            (addr, spk)
        });
        (s, targets.to_vec())
    }

    #[test]
    fn scan_reports_used_addresses_and_only_unspent_outputs() {
        let (store, targets) = scan_fixture();
        let plan = scan_plan(&targets, 50, &store, &Mempool::default(), Network::Bitcoin).unwrap();

        // a and b appear in history; d never does.
        assert_eq!(plan.used, vec![targets[0].0.clone(), targets[1].0.clone()]);

        // a's aa:0 was spent by cc; everything else is still unspent.
        let mut got: Vec<(String, String, u64)> = plan
            .utxos
            .iter()
            .map(|u| (u["address"].as_str().unwrap().into(), u["txid"].as_str().unwrap().into(), u["value"].as_u64().unwrap()))
            .collect();
        got.sort();
        let mut want = vec![
            (targets[0].0.clone(), txid("bb"), 200),
            (targets[1].0.clone(), txid("aa"), 300),
            (targets[1].0.clone(), txid("bb"), 100),
        ];
        want.sort();
        assert_eq!(got, want);
        assert!(plan.utxos.iter().all(|u| u["status"]["confirmed"] == true));
    }

    #[test]
    fn scan_lists_each_tx_once_newest_first_even_when_it_touches_two_addresses() {
        let (store, targets) = scan_fixture();
        let plan = scan_plan(&targets, 50, &store, &Mempool::default(), Network::Bitcoin).unwrap();
        let ids: Vec<&str> = plan.confirmed.iter().map(|h| h.txid.as_str()).collect();
        // bb pays both a and b (listed once); cc is a's spend (sender-side history).
        assert_eq!(ids, vec![txid("cc"), txid("bb"), txid("aa")]);
        assert_eq!(plan.confirmed.iter().map(|h| h.height).collect::<Vec<_>>(), vec![102, 101, 100]);
    }

    #[test]
    fn scan_caps_history_to_the_newest_n_overall() {
        let (store, targets) = scan_fixture();
        let plan = scan_plan(&targets, 2, &store, &Mempool::default(), Network::Bitcoin).unwrap();
        let ids: Vec<&str> = plan.confirmed.iter().map(|h| h.txid.as_str()).collect();
        assert_eq!(ids, vec![txid("cc"), txid("bb")]);
    }

    fn row(txid: &str, height: u64, block: &str) -> HistTx {
        HistTx { txid: txid.into(), height, block_hash: block.into() }
    }

    /// A fake node: counts calls, and returns a tx that records which row it was for.
    fn counting_fetch(calls: &std::sync::atomic::AtomicUsize) -> impl Fn(&HistTx) -> Result<Value> + Sync + '_ {
        move |h| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "txid": h.txid, "status": { "block_height": h.height } }))
        }
    }

    #[test]
    fn confirmed_txs_fetches_each_miss_once_keeps_order_and_then_serves_from_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let cache = TxCache::default();
        let calls = AtomicUsize::new(0);
        // more rows than RPC_PARALLEL, so several groups run
        let rows: Vec<HistTx> = (0..20).map(|i| row(&format!("t{i}"), 100 + i, "blk")).collect();

        let first = confirmed_txs(&rows, &cache, counting_fetch(&calls)).unwrap();
        assert_eq!(calls.load(SeqCst), 20);
        let ids: Vec<&str> = first.iter().map(|t| t["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, rows.iter().map(|r| r.txid.as_str()).collect::<Vec<_>>(), "order preserved");

        let second = confirmed_txs(&rows, &cache, counting_fetch(&calls)).unwrap();
        assert_eq!(calls.load(SeqCst), 20, "a repeat refresh makes no node RPCs");
        assert_eq!(first, second);
    }

    #[test]
    fn a_tx_re_mined_in_another_block_is_a_cache_miss_not_stale_data() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let cache = TxCache::default();
        let calls = AtomicUsize::new(0);
        confirmed_txs(&[row("t1", 100, "blockA")], &cache, counting_fetch(&calls)).unwrap();
        // a reorg re-mines t1 at another height in another block
        let after = confirmed_txs(&[row("t1", 101, "blockB")], &cache, counting_fetch(&calls)).unwrap();
        assert_eq!(calls.load(SeqCst), 2);
        assert_eq!(after[0]["status"]["block_height"], 101);
    }

    #[test]
    fn a_failed_fetch_fails_the_whole_call_and_caches_nothing_for_the_failed_row() {
        let cache = TxCache::default();
        let err = confirmed_txs(&[row("bad", 1, "b")], &cache, |_| Err(anyhow!("node down")));
        assert!(err.is_err());
        assert!(cache.get(&("bad".into(), "b".into())).is_none());
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = TxCache::default();
        for i in 0..=TX_CACHE_MAX {
            cache.insert((format!("t{i}"), "b".into()), json!(i));
        }
        assert!(cache.0.lock().unwrap().len() <= TX_CACHE_MAX);
    }

    #[test]
    fn scan_route_rejects_bad_input_before_touching_the_index() {
        let store = Store::open(&test_store_path()).unwrap();
        let mp = Mempool::default();
        let rpc = Rpc::new("http://127.0.0.1:1", "u:p");
        let cache = TxCache::default();
        let status = |body: &str| match scan_route(body, &store, &rpc, &mp, Network::Bitcoin, &cache) {
            Reply::Json(s, _) => s,
            _ => panic!("expected a JSON reply"),
        };
        assert_eq!(status("not json"), 400);
        assert_eq!(status(r#"{"nope":1}"#), 400);
        assert_eq!(status(r#"{"addresses":[]}"#), 400);
        assert_eq!(status(r#"{"addresses":[7]}"#), 400);
        assert_eq!(status(r#"{"addresses":["not-an-address"]}"#), 400);
        // valid mainnet P2PKH: real address, but not something this index stores
        assert_eq!(status(r#"{"addresses":["1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"]}"#), 400);
    }
}
