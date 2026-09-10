//! The Esplora-shaped REST surface the fortis wallet's `EsploraBackend` calls:
//! `/blocks/tip/height`, `/address/:a/utxo`, `/address/:a/txs`,
//! `/v1/fees/recommended`, and `POST /tx`. Public chain data only — no auth; bind
//! to localhost or a trusted network, or front it with a TLS/rate-limiting proxy.

use std::io::Read;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, ScriptBuf};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use fortis_node::Rpc;

use crate::mempool::Mempool;
use crate::store::Store;

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
    for mut req in server.incoming_requests() {
        let body = read_body(&mut req);
        let mp = mempool.read().unwrap();
        let reply = route(&req, &store, &rpc, &mp, network, &body);
        drop(mp);
        let _ = respond(req, reply);
    }
    Ok(())
}

fn route(
    req: &Request,
    store: &Store,
    rpc: &Rpc,
    mp: &Mempool,
    network: Network,
    body: &str,
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
        (Method::Get, p) if p.starts_with("/address/") => {
            address_route(p, store, rpc, mp, network)
        }
        _ => err(404, "no such route"),
    }
}

fn address_route(path: &str, store: &Store, rpc: &Rpc, mp: &Mempool, network: Network) -> Reply {
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
        "txs" => match address_txs(&spk, store, rpc, mp, network) {
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
) -> Result<Value> {
    let mut txs: Vec<Value> = mp
        .txs_for(spk)
        .into_iter()
        .map(|t| esplora_tx(&backfill_prevouts(t, store, mp, network), None))
        .collect();
    let pending: std::collections::HashSet<String> =
        txs.iter().filter_map(|t| t["txid"].as_str().map(str::to_string)).collect();

    for h in store.history_for(spk, 100)? {
        if pending.contains(&h.txid) {
            continue; // a block just landed that the mempool snapshot still lists
        }
        let t = rpc
            .call("getrawtransaction", json!([h.txid, 2, h.block_hash]))
            .with_context(|| format!("getrawtransaction {}", h.txid))?;
        txs.push(esplora_tx(&t, Some(h.height)));
    }
    Ok(Value::Array(txs))
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
    use crate::store::{IndexedTx, Store, TxOut};

    // BIP-173 P2WPKH example program.
    const P2WPKH_SPK: &str = "0014751e76e8199196d454941c45d1b3a323f1433bd6";
    const P2WPKH_ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

    #[test]
    fn spk_hex_to_address_round_trips_a_p2wpkh() {
        assert_eq!(spk_hex_to_address(P2WPKH_SPK, Network::Bitcoin).as_deref(), Some(P2WPKH_ADDR));
        assert_eq!(spk_hex_to_address("not-hex", Network::Bitcoin), None);
        assert_eq!(spk_hex_to_address("00", Network::Bitcoin), None); // not a known template
    }

    #[test]
    fn backfill_fills_a_null_mempool_prevout_from_the_confirmed_index() {
        let mut store = Store::open(":memory:").unwrap();
        store
            .apply_block(
                100,
                "h100",
                &[IndexedTx {
                    txid: "aa".into(),
                    inputs: vec![],
                    outputs: vec![TxOut { spk_hex: P2WPKH_SPK.into(), value_sat: 500_000 }],
                }],
            )
            .unwrap();
        let mp = Mempool::default();

        // a mempool tx spending aa:0 that the node reported with no prevout
        let pending = json!({
            "txid": "bb",
            "vin": [{ "txid": "aa", "vout": 0 }],
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
        let store = Store::open(":memory:").unwrap();
        let mp = Mempool::default();
        let tx = json!({
            "txid": "bb",
            "vin": [{
                "txid": "aa", "vout": 0,
                "prevout": { "scriptPubKey": { "address": "bc1qkeep" }, "value": 0.001 }
            }],
            "vout": [],
        });
        let filled = backfill_prevouts(&tx, &store, &mp, Network::Bitcoin);
        assert_eq!(filled["vin"][0]["prevout"]["scriptPubKey"]["address"], "bc1qkeep");
    }
}
