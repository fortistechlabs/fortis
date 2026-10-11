//! End-to-end P2P BTC ⇄ XBT swaps through `fortis-edge --swap`.
//!
//! Two `wallet-core` users talk to the edge over HTTP and drive a
//! [`SwapMachine`] the way the phone's `SwapEngine` does (see [`drive`]):
//! offer, match, review the terms, swap keys and the hashlock over the relay,
//! fund with the 0.5% commission output, hand the watchtower its jobs, and
//! redeem or refund through `POST /<chain>/tx`.
//!
//! Chains:
//! - XBT: a Bitcoin Knots BLAKE2b regtest node behind `fortis-index`.
//! - BTC: a stock Bitcoin Core regtest node (`-txindex=1`) behind an in-test
//!   Esplora shim ([`EsploraShim`]) over its RPC.
//!
//! Price: a stub NeoxEX ([`StubNeoxex`]) with a settable price.
//!
//! The five swap scenarios are opt-in: they are skipped (with a note) unless
//! both `FORTIS_BITCOIND` (Knots, BLAKE2b) and `FORTIS_BTC_BITCOIND` (Core)
//! are set. The `harness_*` tests prove the shim, the stub and the driver on
//! their own: the shim test needs only `FORTIS_BTC_BITCOIND`; the stub and
//! driver tests need no node at all.
//!
//! ```sh
//! cargo build --workspace
//! FORTIS_BITCOIND=/opt/bitcoin-knots/current/bin/bitcoind \
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-edge --test swap_e2e -- --nocapture
//! ```
//!
//! The edge runs with the hidden test-only flags `--swap-tick-secs 1` (a
//! watchtower pass every second) and, for the refund scenario, is restarted
//! with `--swap-clock-offset-secs` so the watchtower's clock reaches the XBT
//! locktime that `setmocktime` moved the node's chain past.

mod common;

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response, Server};
use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::hashes::{sha256, Hash};
use wallet_core::bitcoin::secp256k1::PublicKey;
use wallet_core::bitcoin::{consensus, Address, Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use wallet_core::htlc::preimage_from_witness;
use wallet_core::swap_terms::participant_fund_latest;
use wallet_core::{
    btc_for_xbt, params_from_terms, validate_terms, Chain, ChainParams, MasterKey, ServiceFee, SwapEvent,
    SwapKey, SwapMachine, SwapRole, SwapTerms, Utxo, WalletView,
};

use common::{agent, bin, free_port, poll, req, spawn, start_node, wait_up, Flavor, Node, Rpc, Svc, TempDir};

/// 0.01 BTC per XBT.
const PRICE: u64 = 1_000_000;
const XBT_AMT: u64 = 20_000_000;
/// `btc_for_xbt(XBT_AMT, PRICE)`.
const BTC_AMT: u64 = 200_000;
/// sat/vB for every transaction the users build.
const FEERATE: u64 = 2;

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn tx_hex(tx: &Transaction) -> String {
    hex::encode(consensus::serialize(tx))
}

fn decode_tx(h: &str) -> Transaction {
    consensus::deserialize(&hex::decode(h.trim()).expect("tx hex")).expect("tx decode")
}

fn spk_of(addr: &str) -> ScriptBuf {
    Address::<NetworkUnchecked>::from_str(addr).unwrap().assume_checked().script_pubkey()
}

fn commission(sat: u64) -> u64 {
    sat * 50 / 10_000
}

// ==========================================================================
// HTTP stubs: one tiny_http server, a handler, a few threads
// ==========================================================================

type Handler = dyn Fn(&Method, &str, &str) -> (u16, String) + Send + Sync;

/// A local HTTP server answering with `handler(method, url, body)`.
struct Stub {
    url: String,
    server: Arc<Server>,
    threads: usize,
}

impl Stub {
    fn start(handler: Arc<Handler>) -> Stub {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let threads = 4;
        for _ in 0..threads {
            let (server, handler) = (Arc::clone(&server), Arc::clone(&handler));
            std::thread::spawn(move || {
                for mut rq in server.incoming_requests() {
                    let mut body = String::new();
                    let _ = rq.as_reader().read_to_string(&mut body);
                    let (status, text) = handler(rq.method(), rq.url(), &body);
                    let ct = if text.starts_with('{') || text.starts_with('[') { "application/json" } else { "text/plain" };
                    let resp = Response::from_string(text)
                        .with_status_code(status)
                        .with_header(Header::from_bytes("Content-Type", ct).unwrap());
                    let _ = rq.respond(resp);
                }
            });
        }
        Stub { url: format!("http://127.0.0.1:{port}"), server, threads }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        for _ in 0..self.threads {
            self.server.unblock();
        }
    }
}

// ==========================================================================
// Esplora shim over a Bitcoin Core node's RPC
// ==========================================================================

/// The Esplora routes the edge (proxy and watchtower) reads for BTC, served
/// from a Core node with `-txindex=1`:
/// - `GET /tx/{txid}`, `/tx/{txid}/hex`;
/// - `GET /tx/{txid}/outspend/{vout}`: a mempool spend via
///   `gettxspendingprevout`, else a confirmed one by scanning blocks from the
///   funding height;
/// - `GET /blocks/tip/height`, `/fee-estimates`;
/// - `POST /tx`.
struct EsploraShim {
    stub: Stub,
}

impl EsploraShim {
    fn start(rpc: Rpc) -> EsploraShim {
        EsploraShim { stub: Stub::start(Arc::new(move |m: &Method, url: &str, body: &str| shim_reply(&rpc, m, url, body))) }
    }

    fn url(&self) -> &str {
        &self.stub.url
    }
}

fn not_found() -> (u16, String) {
    (404, "Transaction not found".into())
}

fn shim_reply(rpc: &Rpc, method: &Method, url: &str, body: &str) -> (u16, String) {
    let path = url.split('?').next().unwrap_or("").trim_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    let res = match (method, parts.as_slice()) {
        (Method::Get, ["blocks", "tip", "height"]) => rpc.call("getblockcount", json!([])).map(|v| (200, v.to_string())),
        (Method::Get, ["fee-estimates"]) => Ok((200, json!({ "1": 2.0, "2": 2.0, "3": 2.0, "6": 2.0, "144": 1.0 }).to_string())),
        (Method::Post, ["tx"]) => Ok(match rpc.call("sendrawtransaction", json!([body.trim()])) {
            Ok(txid) => (200, txid.as_str().unwrap_or_default().to_string()),
            Err(e) => (400, format!("sendrawtransaction RPC error: {e}")),
        }),
        (Method::Get, ["tx", txid]) => Ok(shim_tx(rpc, txid)),
        (Method::Get, ["tx", txid, "hex"]) => Ok(match rpc.call("getrawtransaction", json!([txid, false])) {
            Ok(h) => (200, h.as_str().unwrap_or_default().to_string()),
            Err(_) => not_found(),
        }),
        (Method::Get, ["tx", txid, "outspend", vout]) => match vout.parse::<u32>() {
            Ok(vout) => shim_outspend(rpc, txid, vout),
            Err(_) => Ok((400, "bad vout".into())),
        },
        _ => Ok((404, "no such route".into())),
    };
    res.unwrap_or_else(|e| (500, format!("rpc: {e}")))
}

/// Esplora `status` for a verbose `getrawtransaction` result.
fn shim_status(rpc: &Rpc, v: &Value) -> Result<Value, String> {
    match v["blockhash"].as_str() {
        Some(hash) => {
            let hdr = rpc.call("getblockheader", json!([hash]))?;
            Ok(json!({ "confirmed": true, "block_height": hdr["height"], "block_hash": hash, "block_time": hdr["time"] }))
        }
        None => Ok(json!({ "confirmed": false })),
    }
}

fn shim_tx(rpc: &Rpc, txid: &str) -> (u16, String) {
    let Ok(v) = rpc.call("getrawtransaction", json!([txid, true])) else {
        return not_found();
    };
    let tx = decode_tx(v["hex"].as_str().unwrap_or_default());
    let status = match shim_status(rpc, &v) {
        Ok(s) => s,
        Err(e) => return (500, e),
    };
    let body = json!({
        "txid": txid,
        "version": tx.version.0,
        "locktime": tx.lock_time.to_consensus_u32(),
        "vin": tx.input.iter().map(|i| json!({
            "txid": i.previous_output.txid.to_string(),
            "vout": i.previous_output.vout,
        })).collect::<Vec<_>>(),
        "vout": tx.output.iter().map(|o| json!({
            "scriptpubkey": hex::encode(o.script_pubkey.as_bytes()),
            "value": o.value.to_sat(),
        })).collect::<Vec<_>>(),
        "status": status,
    });
    (200, body.to_string())
}

fn shim_outspend(rpc: &Rpc, txid: &str, vout: u32) -> Result<(u16, String), String> {
    let Ok(funding) = rpc.call("getrawtransaction", json!([txid, true])) else {
        return Ok(not_found());
    };
    let input_index = |spender: &Value| -> Option<usize> {
        spender["vin"]
            .as_array()?
            .iter()
            .position(|i| i["txid"].as_str() == Some(txid) && i["vout"].as_u64() == Some(vout as u64))
    };
    // A spend still in the mempool.
    let pending = rpc.call("gettxspendingprevout", json!([[{ "txid": txid, "vout": vout }]]))?;
    if let Some(sp) = pending[0]["spendingtxid"].as_str() {
        let spender = rpc.call("getrawtransaction", json!([sp, true]))?;
        let body = json!({ "spent": true, "txid": sp, "vin": input_index(&spender), "status": { "confirmed": false } });
        return Ok((200, body.to_string()));
    }
    let unspent = json!({ "spent": false }).to_string();
    if !rpc.call("gettxout", json!([txid, vout, true]))?.is_null() {
        return Ok((200, unspent));
    }
    // A confirmed spend: scan blocks from the funding height to the tip.
    let Some(hash) = funding["blockhash"].as_str() else {
        return Ok((200, unspent));
    };
    let start = rpc.call("getblockheader", json!([hash]))?["height"].as_u64().unwrap_or(0);
    let tip = rpc.call("getblockcount", json!([]))?.as_u64().unwrap_or(0);
    for h in start..=tip {
        let bh = rpc.call("getblockhash", json!([h]))?;
        let block = rpc.call("getblock", json!([bh, 2]))?;
        for t in block["tx"].as_array().into_iter().flatten() {
            if let Some(i) = input_index(t) {
                let body = json!({
                    "spent": true,
                    "txid": t["txid"],
                    "vin": i,
                    "status": { "confirmed": true, "block_height": h, "block_hash": bh },
                });
                return Ok((200, body.to_string()));
            }
        }
    }
    Ok((200, unspent))
}

// ==========================================================================
// stub NeoxEX
// ==========================================================================

/// Serves `BTCB2_BTC` with a settable price (sat of BTC per XBT): a one-level
/// book ±0.1% around it, a ticker last at it computed "now", and the pair
/// listed unpaused.
struct StubNeoxex {
    stub: Stub,
    price: Arc<AtomicU64>,
}

impl StubNeoxex {
    fn start(price_sat_per_xbt: u64) -> StubNeoxex {
        let price = Arc::new(AtomicU64::new(price_sat_per_xbt));
        let p = Arc::clone(&price);
        let stub = Stub::start(Arc::new(move |_: &Method, url: &str, _: &str| neoxex_reply(p.load(Ordering::SeqCst), url)));
        StubNeoxex { stub, price }
    }

    fn set_price(&self, price_sat_per_xbt: u64) {
        self.price.store(price_sat_per_xbt, Ordering::SeqCst);
    }

    fn url(&self) -> &str {
        &self.stub.url
    }
}

fn neoxex_reply(price_sat: u64, url: &str) -> (u16, String) {
    let p = price_sat as f64 / 1e8;
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let body = match url.split('?').next().unwrap_or("") {
        "/api/exchange/orderbook/BTCB2_BTC" => json!({
            "success": true, "pair": "BTCB2_BTC",
            "bids": [{ "price": p * 0.999, "quantity": 1000.0 }],
            "asks": [{ "price": p * 1.001, "quantity": 1000.0 }],
        }),
        "/api/exchange/ticker/BTCB2_BTC" => json!({
            "success": true, "pair": "BTCB2_BTC",
            "ticker": { "lastPrice": p, "computedAt": now_ms },
        }),
        "/api/exchange/pairs" => json!({ "success": true, "pairs": [{ "pair": "BTCB2_BTC", "tradingPaused": false }] }),
        _ => return (404, "not found".into()),
    };
    (200, body.to_string())
}

// ==========================================================================
// the edge
// ==========================================================================

struct Edge {
    url: String,
    args: Vec<String>,
    log_dir: std::path::PathBuf,
    svc: Option<Svc>,
    restarts: u32,
}

impl Edge {
    /// `fortis-edge --swap` on regtest with a 1 s watchtower tick.
    fn start(dir: &TempDir, xbt_upstream: Option<&str>, btc_upstream: &str, neoxex: &str, fee_address: &str) -> Edge {
        let port = free_port();
        let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
        let mut args: Vec<String> = vec![
            "--bind".into(), format!("127.0.0.1:{port}"),
            "--btc-upstream".into(), btc_upstream.into(),
            // No public fallback (the default is mempool.space).
            "--btc-upstream-fallback=".into(),
            "--secret-file".into(), p("edge.secret"),
            "--network".into(), "regtest".into(),
            "--service-fee-address".into(), fee_address.into(),
            "--swap".into(),
            "--swap-db".into(), p("swap-db"),
            "--neoxex-url".into(), neoxex.into(),
            "--swap-tick-secs".into(), "1".into(),
            "--workers".into(), "8".into(),
            "--rate-per-min".into(), "60000".into(),
            "--rate-burst".into(), "10000".into(),
            "--cache-dir".into(), p("edge-cache"),
            "--btc-history-dir".into(), p("edge-btc-history"),
            "--btc-scan-cache-dir".into(), p("edge-btc-scan-cache"),
        ];
        if let Some(x) = xbt_upstream {
            args.extend(["--xbt-upstream".into(), x.into()]);
        }
        let mut e = Edge { url: format!("http://127.0.0.1:{port}"), args, log_dir: dir.0.clone(), svc: None, restarts: 0 };
        e.launch(&[]);
        e
    }

    fn launch(&mut self, extra: &[&str]) {
        let mut args: Vec<&str> = self.args.iter().map(String::as_str).collect();
        args.extend(extra);
        let log = self.log_dir.join(format!("edge-{}.log", self.restarts));
        self.svc = Some(spawn(&bin("fortis-edge"), &args, Some(log)));
        wait_up(&agent(), &format!("{}/", self.url));
    }

    /// Stops the edge and starts it again on the same port and data, with
    /// `extra` flags (the swap store, tokens and watch jobs persist).
    fn restart(&mut self, extra: &[&str]) {
        self.svc = None;
        std::thread::sleep(Duration::from_millis(300));
        self.restarts += 1;
        self.launch(extra);
    }
}

// ==========================================================================
// the users and the driver
// ==========================================================================

/// What one user drives, in the order `SwapEngine` does it.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// `POST /swap/offers` (`direction`, `amount_sat`).
    Offer(&'static str, u64),
    /// Wait for the offer's match; read the terms and our role.
    AwaitMatch,
    /// Check the terms against our own price and the edge's `/pricing`
    /// address; accept and send our keys (the initiator also the hashlock),
    /// or decline.
    Review,
    /// Read the relay until the counterparty's keys (and the hashlock) are
    /// in; build the `SwapMachine`.
    AwaitKeys,
    /// Fund our HTLC with the 0.5% commission output. The participant first
    /// waits for the XBT HTLC to have `xbt_confs` and verifies it exactly.
    Fund,
    /// Hand the watchtower our signed refunds (not before our locktime).
    WatchRefund,
    /// Participant: hand the watchtower presigned XBT redeems.
    WatchRedeem,
    /// Initiator: redeem the BTC HTLC with the preimage once it has
    /// `btc_confs`. Participant: learn the preimage from that redeem and
    /// redeem the XBT HTLC. Both through `POST /<chain>/tx`.
    Redeem,
    /// Refund our own HTLC through `POST /<chain>/tx`.
    Refund,
}

#[derive(Default)]
struct Session {
    offer_id: String,
    secret: String,
    match_id: String,
    role: Option<SwapRole>,
    terms: Option<SwapTerms>,
    after: u64,
    their_keys: Option<(PublicKey, PublicKey)>,
    hashlock: Option<sha256::Hash>,
    accepted: Option<bool>,
    decline_status: Option<u16>,
    machine: Option<SwapMachine>,
    funding_tx: Option<Transaction>,
    /// Output values of the redeems handed to the watchtower.
    watched_redeem_values: Vec<u64>,
    /// The last redeem or refund, and the edge's answer to `POST /<chain>/tx`.
    spend: Option<(Transaction, u16, String)>,
}

struct User {
    name: &'static str,
    key: MasterKey,
    xbt: ChainParams,
    btc: ChainParams,
    a: ureq::Agent,
    edge: String,
    token: String,
    /// The user's own view of the price (the phone reads NeoxEX itself).
    own_price: u64,
    slippage: u32,
    swap_index: u32,
    coins: Vec<(Chain, Utxo)>,
    s: Session,
}

impl User {
    /// A user with an install token from `edge`.
    fn new(name: &'static str, seed: u8, edge: &str, own_price: u64) -> User {
        let mut u = User::offline(name, seed, own_price);
        u.edge = edge.to_string();
        let (s, body) = req(&u.a, "POST", &format!("{edge}/register"), &[], Some(""));
        assert_eq!(s, 200, "register: {body}");
        u.token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
        u
    }

    /// Keys and coins only; no edge yet.
    fn offline(name: &'static str, seed: u8, own_price: u64) -> User {
        User {
            name,
            key: MasterKey::generate(&[seed; 32]).unwrap().1,
            xbt: ChainParams::blake2b_regtest(),
            btc: ChainParams::bitcoin_regtest(),
            a: agent(),
            edge: String::new(),
            token: String::new(),
            own_price,
            slippage: 100,
            swap_index: 0,
            coins: Vec::new(),
            s: Session::default(),
        }
    }

    fn params(&self, chain: Chain) -> &ChainParams {
        match chain {
            Chain::Xbt => &self.xbt,
            Chain::Btc => &self.btc,
        }
    }

    fn view(&self, chain: Chain) -> WalletView {
        let p = self.params(chain);
        WalletView::new(p.clone(), self.key.account_xpub(p, 0).unwrap())
    }

    fn address(&self, chain: Chain, index: u32) -> Address {
        self.view(chain).address_at(0, index).unwrap()
    }

    /// Where redeems and refunds pay out.
    fn payout(&self, chain: Chain) -> Address {
        self.address(chain, 7)
    }

    fn swap_key(&self, chain: Chain) -> SwapKey {
        let p = self.params(chain);
        SwapKey::from_secret(p, self.key.swap_secret(p, 0, self.swap_index).unwrap()).unwrap()
    }

    /// Our `(xbt, btc)` swap pubkeys.
    fn pubkeys(&self) -> (PublicKey, PublicKey) {
        (self.swap_key(Chain::Xbt).pubkey(), self.swap_key(Chain::Btc).pubkey())
    }

    fn role(&self) -> SwapRole {
        self.s.role.expect("no role yet")
    }

    fn terms(&self) -> SwapTerms {
        self.s.terms.clone().expect("no terms yet")
    }

    fn send_chain(&self) -> Chain {
        match self.role() {
            SwapRole::Initiator => Chain::Xbt,
            SwapRole::Participant => Chain::Btc,
        }
    }

    fn machine(&mut self) -> &mut SwapMachine {
        self.s.machine.as_mut().expect("no machine yet")
    }

    fn http(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let auth = format!("Bearer {}", self.token);
        let mut headers = vec![("Authorization", auth.as_str())];
        if !self.s.secret.is_empty() {
            headers.push(("X-Swap-Secret", self.s.secret.as_str()));
        }
        req(&self.a, method, &format!("{}{path}", self.edge), &headers, body)
    }

    /// A 200 JSON answer, or a panic naming the user and route.
    fn json(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let b = body.map(|v| v.to_string());
        let (s, text) = self.http(method, path, b.as_deref());
        assert_eq!(s, 200, "{}: {method} {path}: {text}", self.name);
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {path}: {e}: {text}", self.name))
    }

    fn message(&self, body: Value) -> (u16, String) {
        self.http("POST", &format!("/swap/matches/{}/messages", self.s.match_id), Some(&body.to_string()))
    }

    fn get_match(&self) -> Value {
        self.json("GET", &format!("/swap/matches/{}", self.s.match_id), None)
    }

    /// `POST /<chain>/tx` through the edge.
    fn broadcast(&self, chain: Chain, tx: &Transaction) -> (u16, String) {
        self.http("POST", &format!("/{}/tx", chain.as_str()), Some(&tx_hex(tx)))
    }

    /// Spendable coins on `chain`, best first.
    fn coins_on(&self, chain: Chain) -> Vec<Utxo> {
        self.coins.iter().filter(|(c, _)| *c == chain).map(|(_, u)| u.clone()).collect()
    }

    /// A signed payment of `outputs` from our coins on `chain`, with the
    /// service fee when given, and the coins it spends.
    fn sign_payment(&self, chain: Chain, outputs: Vec<TxOut>, fee: Option<&ServiceFee>) -> (Transaction, Vec<OutPoint>) {
        let coins = self.coins_on(chain);
        let p = self.params(chain);
        let plan = self.view(chain).plan_payment(&coins, outputs, FEERATE, 1, fee, false).unwrap();
        let mut tx = plan.tx.clone();
        let prevouts: Vec<TxOut> =
            plan.selected.iter().map(|u| TxOut { value: u.value, script_pubkey: u.script_pubkey.clone() }).collect();
        let paths: Vec<(bool, u32)> = plan.selected.iter().map(|u| (u.is_change, u.derivation_index)).collect();
        self.key.sign_p2wpkh_tx(p, 0, &mut tx, &prevouts, &paths).unwrap();
        (tx, plan.selected.iter().map(|u| u.outpoint).collect())
    }

    fn spent(&mut self, chain: Chain, spent: &[OutPoint]) {
        self.coins.retain(|(c, u)| !(*c == chain && spent.contains(&u.outpoint)));
    }

    /// [`Self::sign_payment`], broadcast through the edge. Spent coins are dropped.
    fn pay(&mut self, chain: Chain, outputs: Vec<TxOut>, fee: Option<&ServiceFee>) -> (Transaction, u16, String) {
        let (tx, spent) = self.sign_payment(chain, outputs, fee);
        let (s, body) = self.broadcast(chain, &tx);
        if s == 200 {
            self.spent(chain, &spent);
        }
        (tx, s, body)
    }
}

/// Runs `steps` for `u`, in order.
fn drive(u: &mut User, steps: &[Step]) {
    for s in steps {
        step(u, *s);
    }
}

fn step(u: &mut User, s: Step) {
    match s {
        Step::Offer(direction, amount_sat) => {
            u.s = Session::default();
            u.swap_index += 1;
            let v = u.json(
                "POST",
                "/swap/offers",
                Some(json!({ "direction": direction, "amount_sat": amount_sat, "max_slippage_bps": u.slippage })),
            );
            u.s.offer_id = v["offer_id"].as_str().unwrap().to_string();
            u.s.secret = v["owner_secret"].as_str().unwrap().to_string();
        }
        Step::AwaitMatch => {
            let path = format!("/swap/offers/{}", u.s.offer_id);
            u.s.match_id = poll("a match", 30, || u.json("GET", &path, None)["match_id"].as_str().map(String::from));
            let m = u.get_match();
            u.s.terms = Some(serde_json::from_value(m["terms"].clone()).unwrap());
            u.s.role = Some(match m["role"].as_str().unwrap() {
                "initiator" => SwapRole::Initiator,
                "participant" => SwapRole::Participant,
                other => panic!("role {other}"),
            });
        }
        Step::Review => {
            let t = u.terms();
            // The commission must go to the edge's advertised fee address.
            let pricing = u.json("GET", "/pricing", None);
            let fee_ok = pricing["address"].as_str() == Some(t.fee_address.as_str());
            let valid = validate_terms(&t, u.own_price, u.slippage, u.role(), unix_now() as u32);
            if valid.is_ok() && fee_ok {
                let (s, b) = u.message(json!({ "type": "accept" }));
                assert_eq!(s, 200, "{}: accept: {b}", u.name);
                let (x, b) = u.pubkeys();
                let (s, body) = u.message(json!({ "type": "keys", "xbt": hex::encode(x.serialize()), "btc": hex::encode(b.serialize()) }));
                assert_eq!(s, 200, "{}: keys: {body}", u.name);
                if u.role() == SwapRole::Initiator {
                    let h = sha256::Hash::hash(&u.swap_key(Chain::Xbt).preimage());
                    let (s, body) = u.message(json!({ "type": "hashlock", "hex": hex::encode(h.to_byte_array()) }));
                    assert_eq!(s, 200, "{}: hashlock: {body}", u.name);
                    u.s.hashlock = Some(h);
                }
                u.s.accepted = Some(true);
            } else {
                eprintln!("{}: declining: {valid:?}, fee address ok: {fee_ok}", u.name);
                let (s, _) = u.message(json!({ "type": "decline" }));
                u.s.decline_status = Some(s);
                u.s.accepted = Some(false);
            }
        }
        Step::AwaitKeys => {
            let them = match u.role() {
                SwapRole::Initiator => "participant",
                SwapRole::Participant => "initiator",
            };
            poll("the counterparty's keys and the hashlock", 60, || {
                let v = u.json("GET", &format!("/swap/matches/{}/messages?after={}", u.s.match_id, u.s.after), None);
                for m in v["messages"].as_array().unwrap() {
                    u.s.after = u.s.after.max(m["seq"].as_u64().unwrap());
                    let (from, body) = (m["from"].as_str().unwrap(), &m["body"]);
                    match body["type"].as_str().unwrap() {
                        "keys" if from == them => {
                            let k = |f: &str| PublicKey::from_str(body[f].as_str().unwrap()).unwrap();
                            u.s.their_keys = Some((k("xbt"), k("btc")));
                        }
                        "hashlock" if from == "initiator" => {
                            let h: [u8; 32] = hex::decode(body["hex"].as_str().unwrap()).unwrap().try_into().unwrap();
                            u.s.hashlock = Some(sha256::Hash::from_byte_array(h));
                        }
                        "decline" => panic!("{}: the counterparty declined", u.name),
                        _ => {}
                    }
                }
                (u.s.their_keys.is_some() && u.s.hashlock.is_some()).then_some(())
            });
            let t = u.terms();
            let swap_id: [u8; 16] = sha256::Hash::hash(t.match_id.as_bytes()).to_byte_array()[..16].try_into().unwrap();
            let params = params_from_terms(&t, u.role(), u.pubkeys(), u.s.their_keys.unwrap(), u.s.hashlock.unwrap(), swap_id);
            u.s.machine = Some(SwapMachine::new(params));
        }
        Step::Fund => {
            let t = u.terms();
            let chain = u.send_chain();
            if u.role() == SwapRole::Participant {
                // The initiator's XBT HTLC, exactly, with xbt_confs.
                let (txid, vout) = poll("the XBT funding", 60, || {
                    let f = u.get_match()["xbt_funding"].clone();
                    Some((f["txid"].as_str()?.to_string(), f["vout"].as_u64()? as u32))
                });
                let out = poll("the XBT funding confirmations", 90, || {
                    let (s, b) = u.http("GET", &format!("/xbt/txout/{txid}/{vout}"), None);
                    let v: Value = serde_json::from_str(&b).ok().filter(|_| s == 200)?;
                    (v["confirmations"].as_u64()? >= t.xbt_confs as u64).then_some(v)
                });
                let spk = ScriptBuf::from(hex::decode(out["scriptpubkey_hex"].as_str().unwrap()).unwrap());
                let xbt = u.xbt.clone();
                u.machine()
                    .verify_their_output(&spk, Amount::from_sat(out["value_sat"].as_u64().unwrap()), &xbt)
                    .expect("the XBT HTLC must match the agreed contract exactly");
                u.machine().set_their_outpoint(OutPoint::new(Txid::from_str(&txid).unwrap(), vout));
                // Never fund with too little of the BTC leg left (half a short regtest leg).
                assert!(
                    unix_now() <= participant_fund_latest(&t) as u64,
                    "{}: too late to fund the BTC leg",
                    u.name
                );
            }
            let params = u.params(chain).clone();
            let contract = u.machine().build_our_contract(&params).unwrap().clone();
            let fee = ServiceFee { bps: t.fee_bps, floor_sat: 0, cap_sat: 0, fee_spk: spk_of(&t.fee_address) };
            let (tx, s, body) = u.pay(chain, vec![contract.funding_output()], Some(&fee));
            assert_eq!(s, 200, "{}: funding broadcast: {body}", u.name);
            let txid = tx.compute_txid();
            assert_eq!(body.trim(), txid.to_string());
            let vout = tx.output.iter().position(|o| o.script_pubkey == contract.script_pubkey).unwrap() as u32;
            u.machine().set_our_outpoint(OutPoint::new(txid, vout));
            u.machine().on_event(SwapEvent::OurFundingBroadcast { txid });
            let (s, b) = u.message(json!({ "type": "funded", "chain": chain.as_str(), "txid": txid.to_string(), "vout": vout }));
            assert_eq!(s, 200, "{}: funded: {b}", u.name);
            u.s.funding_tx = Some(tx);
        }
        Step::WatchRefund => {
            let t = u.terms();
            let chain = u.send_chain();
            let op = u.s.machine.as_ref().unwrap().our_outpoint().unwrap();
            let (payout, key) = (u.payout(chain).script_pubkey(), u.swap_key(chain));
            let refunds = u.s.machine.as_ref().unwrap().signed_refund(op, payout, &key, &[FEERATE, 10]).unwrap();
            let not_before = match chain {
                Chain::Xbt => t.xbt_locktime,
                Chain::Btc => t.btc_locktime,
            };
            u.json(
                "POST",
                "/swap/watch",
                Some(json!({
                    "match_id": u.s.match_id, "chain": chain.as_str(), "kind": "refund",
                    "htlc_outpoint": { "txid": op.txid.to_string(), "vout": op.vout },
                    "not_before": not_before,
                    "variants": refunds.iter().map(tx_hex).collect::<Vec<_>>(),
                })),
            );
        }
        Step::WatchRedeem => {
            assert_eq!(u.role(), SwapRole::Participant);
            let op = u.s.machine.as_ref().unwrap().their_outpoint().unwrap();
            let (payout, key) = (u.payout(Chain::Xbt).script_pubkey(), u.swap_key(Chain::Xbt));
            let pre = u.s.machine.as_ref().unwrap().presign_redeem(op, payout, &key, &[FEERATE, 10]).unwrap();
            u.s.watched_redeem_values = pre.iter().map(|p| decode_tx(&p.tx_hex).output[0].value.to_sat()).collect();
            u.json(
                "POST",
                "/swap/watch",
                Some(json!({
                    "match_id": u.s.match_id, "chain": "xbt", "kind": "redeem",
                    "htlc_outpoint": { "txid": op.txid.to_string(), "vout": op.vout },
                    "not_before": 0,
                    "variants": pre.iter().map(|p| p.tx_hex.clone()).collect::<Vec<_>>(),
                    "sigs": pre.iter().map(|p| p.sig_hex.clone()).collect::<Vec<_>>(),
                    "pubkey": pre[0].pubkey_hex,
                    "witness_script": pre[0].witness_script_hex,
                })),
            );
        }
        Step::Redeem => match u.role() {
            SwapRole::Initiator => {
                let t = u.terms();
                let (txid, vout) = poll("the BTC funding", 60, || {
                    let f = u.get_match()["btc_funding"].clone();
                    Some((f["txid"].as_str()?.to_string(), f["vout"].as_u64()? as u32))
                });
                let out = poll("the BTC funding confirmations", 90, || {
                    let (s, b) = u.http("GET", &format!("/btc/tx/{txid}"), None);
                    let v: Value = serde_json::from_str(&b).ok().filter(|_| s == 200)?;
                    let h = v["status"]["block_height"].as_u64()?;
                    let (_, tip) = u.http("GET", "/btc/blocks/tip/height", None);
                    let tip: u64 = tip.trim().parse().ok()?;
                    (tip + 1 >= h + t.btc_confs as u64).then(|| v["vout"][vout as usize].clone())
                });
                let spk = ScriptBuf::from(hex::decode(out["scriptpubkey"].as_str().unwrap()).unwrap());
                let btc = u.btc.clone();
                u.machine()
                    .verify_their_output(&spk, Amount::from_sat(out["value"].as_u64().unwrap()), &btc)
                    .expect("the BTC HTLC must match the agreed contract exactly");
                let op = OutPoint::new(Txid::from_str(&txid).unwrap(), vout);
                u.machine().set_their_outpoint(op);
                let preimage = u.swap_key(Chain::Xbt).preimage();
                let (payout, key) = (u.payout(Chain::Btc).script_pubkey(), u.swap_key(Chain::Btc));
                let tx = u.s.machine.as_ref().unwrap().signed_redeem(op, payout, &key, &preimage, &[FEERATE]).unwrap().remove(0);
                let (s, body) = u.broadcast(Chain::Btc, &tx);
                u.s.spend = Some((tx, s, body));
            }
            SwapRole::Participant => {
                let ours = u.s.machine.as_ref().unwrap().our_outpoint().unwrap();
                let spender = poll("the initiator's BTC redeem", 60, || {
                    let (s, b) = u.http("GET", &format!("/btc/tx/{}/outspend/{}", ours.txid, ours.vout), None);
                    let v: Value = serde_json::from_str(&b).ok().filter(|_| s == 200)?;
                    v["txid"].as_str().filter(|_| v["spent"] == true).map(String::from)
                });
                let (s, h) = u.http("GET", &format!("/btc/tx/{spender}/hex"), None);
                assert_eq!(s, 200, "{}: spender hex: {h}", u.name);
                let redeem = decode_tx(&h);
                let preimage = redeem
                    .input
                    .iter()
                    .find(|i| i.previous_output == ours)
                    .and_then(|i| preimage_from_witness(&i.witness))
                    .expect("the BTC redeem reveals the preimage");
                u.machine().on_event(SwapEvent::CounterpartyRedeemed { preimage });
                let op = u.s.machine.as_ref().unwrap().their_outpoint().unwrap();
                let (payout, key) = (u.payout(Chain::Xbt).script_pubkey(), u.swap_key(Chain::Xbt));
                let tx = u.s.machine.as_ref().unwrap().signed_redeem(op, payout, &key, &preimage, &[FEERATE]).unwrap().remove(0);
                let (s, body) = u.broadcast(Chain::Xbt, &tx);
                u.s.spend = Some((tx, s, body));
            }
        },
        Step::Refund => {
            let chain = u.send_chain();
            let op = u.s.machine.as_ref().unwrap().our_outpoint().unwrap();
            let (payout, key) = (u.payout(chain).script_pubkey(), u.swap_key(chain));
            let tx = u.s.machine.as_ref().unwrap().signed_refund(op, payout, &key, &[FEERATE]).unwrap().remove(0);
            let (s, body) = u.broadcast(chain, &tx);
            u.s.spend = Some((tx, s, body));
        }
    }
}

// ==========================================================================
// the world: two chains, the index, the shim, the stub and the edge
// ==========================================================================

fn btc_str(sats: u64) -> String {
    format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}

/// Pays `sats` from the node's miner wallet to `u`'s receive address
/// `index` on `chain`, mines it, and gives `u` the coin.
fn fund_user(node: &Node, u: &mut User, chain: Chain, index: u32, sats: u64) {
    let addr = u.address(chain, index);
    let txid = node.wallet("miner", &["sendtoaddress", &addr.to_string(), &btc_str(sats)]);
    let tx: Value = serde_json::from_str(&node.cli(&["getrawtransaction", &txid, "true"])).unwrap();
    let spk = addr.script_pubkey();
    let vout = tx["vout"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["scriptPubKey"]["hex"].as_str() == Some(hex::encode(spk.as_bytes()).as_str()))
        .unwrap()["n"]
        .as_u64()
        .unwrap() as u32;
    u.coins.push((
        chain,
        Utxo {
            outpoint: OutPoint::new(Txid::from_str(&txid).unwrap(), vout),
            value: Amount::from_sat(sats),
            script_pubkey: spk,
            confirmations: 1,
            derivation_index: index,
            is_change: false,
        },
    ));
    node.mine(1);
}

/// Fields drop in order: the edge first, the nodes and the temp dir last.
struct World {
    edge: Edge,
    _index: Svc,
    _shim: EsploraShim,
    _stub: StubNeoxex,
    knots: Node,
    core: Node,
    fee_spk: ScriptBuf,
    _dir: TempDir,
}

/// Both chains, `fortis-index` over Knots, the Esplora shim over Core, the
/// stub NeoxEX at `price` and `fortis-edge --swap`; `None` (with a note)
/// unless both node binaries are configured.
fn world(price: u64) -> Option<World> {
    let (Ok(knots), Ok(core)) = (std::env::var("FORTIS_BITCOIND"), std::env::var("FORTIS_BTC_BITCOIND")) else {
        eprintln!(
            "skipping: set FORTIS_BITCOIND (Knots, BLAKE2b regtest) and FORTIS_BTC_BITCOIND \
             (Bitcoin Core, run with -txindex=1) to run the swap end-to-end tests"
        );
        return None;
    };
    let dir = TempDir::new("world");
    let knots = start_node(Path::new(&knots), Flavor::Knots);
    let dep = knots.cli(&["getdeploymentinfo"]);
    assert!(dep.contains("\"blake2b\"") && dep.contains("\"active\": true"), "blake2b not active on FORTIS_BITCOIND");
    let core = start_node(Path::new(&core), Flavor::Core);
    knots.mine(110);
    core.mine(110);

    let idx_port = free_port();
    let index = spawn(
        &bin("fortis-index"),
        &[
            "--network", "regtest",
            "--rpc-url", &knots.rpc_url(),
            "--cookie-file", knots.cookie_file().to_str().unwrap(),
            "--db", dir.path("idx-rocksdb").to_str().unwrap(),
            "--start-height", "0",
            "--bind", &format!("127.0.0.1:{idx_port}"),
        ],
        Some(dir.path("index.log")),
    );
    let idx_url = format!("http://127.0.0.1:{idx_port}");
    wait_up(&agent(), &format!("{idx_url}/"));

    let shim = EsploraShim::start(core.rpc());
    let stub = StubNeoxex::start(price);
    let fee_key = MasterKey::generate(&[0xfe; 32]).unwrap().1;
    let p = ChainParams::bitcoin_regtest();
    let fee_addr = WalletView::new(p.clone(), fee_key.account_xpub(&p, 0).unwrap()).address_at(0, 0).unwrap();
    let edge = Edge::start(&dir, Some(&idx_url), shim.url(), stub.url(), &fee_addr.to_string());
    Some(World { edge, _index: index, _shim: shim, _stub: stub, knots, core, fee_spk: fee_addr.script_pubkey(), _dir: dir })
}

impl World {
    /// Alice holds XBT (the initiator), Bob holds BTC (the participant).
    fn users(&self, own_price: u64) -> (User, User) {
        let mut alice = User::new("alice", 0xa1, &self.edge.url, own_price);
        let mut bob = User::new("bob", 0xb0, &self.edge.url, own_price);
        fund_user(&self.knots, &mut alice, Chain::Xbt, 0, 100_000_000);
        fund_user(&self.knots, &mut alice, Chain::Xbt, 1, 50_000_000);
        fund_user(&self.core, &mut bob, Chain::Btc, 0, 1_000_000);
        fund_user(&self.core, &mut bob, Chain::Btc, 1, 500_000);
        (alice, bob)
    }
}

/// Offers both ways, match, review, keys and hashlock: both machines built.
fn negotiate(alice: &mut User, bob: &mut User, xbt: u64, btc: u64) {
    drive(alice, &[Step::Offer("xbt_to_btc", xbt)]);
    drive(bob, &[Step::Offer("btc_to_xbt", btc)]);
    drive(alice, &[Step::AwaitMatch]);
    drive(bob, &[Step::AwaitMatch]);
    drive(alice, &[Step::Review]);
    drive(bob, &[Step::Review]);
    assert_eq!(alice.s.accepted, Some(true), "alice accepts honest terms");
    assert_eq!(bob.s.accepted, Some(true), "bob accepts honest terms");
    assert_eq!(alice.role(), SwapRole::Initiator);
    assert_eq!(bob.role(), SwapRole::Participant);
    drive(alice, &[Step::AwaitKeys]);
    drive(bob, &[Step::AwaitKeys]);
}

/// The funding pays the HTLC exactly the terms' amount and the commission
/// exactly 50 bps of it to the fee address.
fn assert_funding_pays_commission(tx: &Transaction, fee_spk: &ScriptBuf, htlc_spk: &ScriptBuf, amount: u64) {
    let paid = |spk: &ScriptBuf| tx.output.iter().filter(|o| &o.script_pubkey == spk).map(|o| o.value.to_sat()).sum::<u64>();
    assert_eq!(paid(htlc_spk), amount, "HTLC output");
    assert_eq!(paid(fee_spk), commission(amount), "commission output");
}

/// Mines a block on `node` every couple of seconds until `f` holds.
fn mine_until<T>(node: &Node, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..45 {
        if let Some(v) = f() {
            return v;
        }
        node.mine(1);
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!("timed out waiting for {what}");
}

/// The first UTXO at `addr` as the edge reports it, if any.
fn xbt_utxo(u: &User, addr: &Address) -> Option<Value> {
    let (s, b) = u.http("GET", &format!("/xbt/address/{addr}/utxo"), None);
    let v: Value = serde_json::from_str(&b).ok().filter(|_| s == 200)?;
    v.as_array()?.first().cloned()
}

fn htlc_spk(u: &User) -> ScriptBuf {
    let f = u.s.funding_tx.as_ref().unwrap();
    let op = u.s.machine.as_ref().unwrap().our_outpoint().unwrap();
    f.output[op.vout as usize].script_pubkey.clone()
}

// ==========================================================================
// scenarios (opt-in)
// ==========================================================================

#[test]
fn happy_path_both_ways_pays_commission_on_both_chains() {
    let Some(w) = world(PRICE) else { return };
    let (mut alice, mut bob) = w.users(PRICE);
    negotiate(&mut alice, &mut bob, XBT_AMT, BTC_AMT);
    let t = alice.terms();
    assert_eq!((t.xbt_sat, t.btc_sat), (XBT_AMT, BTC_AMT));
    assert_eq!(t.btc_sat, btc_for_xbt(t.xbt_sat, t.price_sat_per_xbt));

    drive(&mut alice, &[Step::Fund, Step::WatchRefund]);
    w.knots.mine(t.xbt_confs);
    // Both stay online: no watchtower redeem for Bob, which would race his own.
    drive(&mut bob, &[Step::Fund]);
    w.core.mine(t.btc_confs);

    assert_funding_pays_commission(alice.s.funding_tx.as_ref().unwrap(), &w.fee_spk, &htlc_spk(&alice), t.xbt_sat);
    assert_funding_pays_commission(bob.s.funding_tx.as_ref().unwrap(), &w.fee_spk, &htlc_spk(&bob), t.btc_sat);

    drive(&mut alice, &[Step::Redeem]);
    let (a_redeem, s, body) = alice.s.spend.clone().unwrap();
    assert_eq!(s, 200, "alice's BTC redeem through /btc/tx: {body}");
    drive(&mut bob, &[Step::Redeem]);
    let (b_redeem, s, body) = bob.s.spend.clone().unwrap();
    assert_eq!(s, 200, "bob's XBT redeem through /xbt/tx: {body}");
    w.core.mine(1);
    w.knots.mine(1);

    // Each side received its leg less only the redeem's network fee.
    let a_got = a_redeem.output[0].value.to_sat();
    assert!(a_got > t.btc_sat - 2_000 && a_got < t.btc_sat, "alice's BTC: {a_got}");
    assert_eq!(a_redeem.output[0].script_pubkey, alice.payout(Chain::Btc).script_pubkey());
    let b_got = b_redeem.output[0].value.to_sat();
    assert!(b_got > t.xbt_sat - 2_000 && b_got < t.xbt_sat, "bob's XBT: {b_got}");
    let utxo = poll("bob's XBT payout", 60, || xbt_utxo(&bob, &bob.payout(Chain::Xbt)));
    assert_eq!(utxo["value"].as_u64(), Some(b_got));
    let confirmed = poll("alice's BTC redeem to confirm", 60, || {
        let (s, b) = alice.http("GET", &format!("/btc/tx/{}", a_redeem.compute_txid()), None);
        let v: Value = serde_json::from_str(&b).ok().filter(|_| s == 200)?;
        (v["status"]["confirmed"] == true).then_some(v)
    });
    assert_eq!(confirmed["vout"][0]["value"].as_u64(), Some(a_got));

    // Both commissions are on chain at the fee address.
    let a_fund = alice.s.funding_tx.as_ref().unwrap();
    let fee_vout = a_fund.output.iter().position(|o| o.script_pubkey == w.fee_spk).unwrap();
    let out = alice.json("GET", &format!("/xbt/txout/{}/{fee_vout}", a_fund.compute_txid()), None);
    assert_eq!(out["value_sat"].as_u64(), Some(commission(t.xbt_sat)));
    let b_fund = bob.s.funding_tx.as_ref().unwrap();
    let fee_vout = b_fund.output.iter().position(|o| o.script_pubkey == w.fee_spk).unwrap();
    let v = bob.json("GET", &format!("/btc/tx/{}", b_fund.compute_txid()), None);
    assert_eq!(v["status"]["confirmed"], true);
    assert_eq!(v["vout"][fee_vout]["value"].as_u64(), Some(commission(t.btc_sat)));

    // The watchtower closes the match once both HTLCs are spent.
    poll("the match to close", 60, || (alice.get_match()["status"] == "closed").then_some(()));
}

#[test]
fn participant_offline_after_funding_watchtower_redeems() {
    let Some(w) = world(PRICE) else { return };
    let (mut alice, mut bob) = w.users(PRICE);
    negotiate(&mut alice, &mut bob, XBT_AMT, BTC_AMT);
    let t = alice.terms();
    drive(&mut alice, &[Step::Fund, Step::WatchRefund]);
    w.knots.mine(t.xbt_confs);
    drive(&mut bob, &[Step::Fund, Step::WatchRedeem]);
    // Bob goes offline here.
    w.core.mine(t.btc_confs);
    drive(&mut alice, &[Step::Redeem]);
    let (_, s, body) = alice.s.spend.clone().unwrap();
    assert_eq!(s, 200, "alice's BTC redeem: {body}");
    w.core.mine(1);

    // The watchtower finishes Bob's presigned XBT redeem with the preimage.
    let payout = bob.payout(Chain::Xbt);
    let utxo = mine_until(&w.knots, "the watchtower's XBT redeem", || {
        xbt_utxo(&bob, &payout).filter(|u| u["status"]["confirmed"] == true)
    });
    let got = utxo["value"].as_u64().unwrap();
    assert!(bob.s.watched_redeem_values.contains(&got), "{got} is one of {:?}", bob.s.watched_redeem_values);
    let ours = bob.s.machine.as_ref().unwrap().their_outpoint().unwrap();
    let (s, _) = bob.http("GET", &format!("/xbt/txout/{}/{}", ours.txid, ours.vout), None);
    assert_eq!(s, 404, "the XBT HTLC is spent");
}

#[test]
fn participant_never_funds_initiator_refunded_by_watchtower() {
    let Some(mut w) = world(PRICE) else { return };
    let (mut alice, mut bob) = w.users(PRICE);
    negotiate(&mut alice, &mut bob, XBT_AMT, BTC_AMT);
    let t = alice.terms();
    drive(&mut alice, &[Step::Fund, Step::WatchRefund]);
    // Alice goes offline; Bob never funds.
    w.knots.mine(t.xbt_confs);
    poll("the participant's deadline", 60, || alice.get_match()["participant_fund_by"].as_u64());

    // Move the XBT chain past the locktime (median time past > locktime), and
    // the watchtower's clock with it.
    let target = t.xbt_locktime as u64 + 120;
    let offset = target - unix_now();
    w.knots.set_mock_time(target);
    w.knots.mine(12);
    w.edge.restart(&["--swap-clock-offset-secs", &offset.to_string()]);

    let payout = alice.payout(Chain::Xbt);
    let utxo = mine_until(&w.knots, "the watchtower's XBT refund", || {
        xbt_utxo(&alice, &payout).filter(|u| u["status"]["confirmed"] == true)
    });
    let got = utxo["value"].as_u64().unwrap();
    assert!(got > t.xbt_sat - 5_000 && got < t.xbt_sat, "alice's refund: {got}");
    // The participant missed its deadline: the match is cancelled.
    poll("the match to be cancelled", 30, || (alice.get_match()["status"] == "cancelled").then_some(()));
}

#[test]
fn skewed_price_both_decline() {
    // The edge quotes 5% above what both phones see on NeoxEX themselves.
    let skewed = PRICE * 105 / 100;
    let Some(w) = world(skewed) else { return };
    let (mut alice, mut bob) = w.users(PRICE);
    drive(&mut alice, &[Step::Offer("xbt_to_btc", XBT_AMT)]);
    drive(&mut bob, &[Step::Offer("btc_to_xbt", btc_for_xbt(XBT_AMT, skewed))]);
    // Both see the match before either reviews it (a decline hides it from the offer).
    drive(&mut alice, &[Step::AwaitMatch]);
    drive(&mut bob, &[Step::AwaitMatch]);
    drive(&mut alice, &[Step::Review]);
    drive(&mut bob, &[Step::Review]);
    assert_declined_and_reopened(&alice, &bob);
}

fn assert_declined_and_reopened(alice: &User, bob: &User) {
    assert!(alice.terms().price_sat_per_xbt.abs_diff(PRICE * 105 / 100) <= 1, "the edge's skewed price");
    assert_eq!(alice.s.accepted, Some(false), "alice declines");
    assert_eq!(bob.s.accepted, Some(false), "bob declines");
    assert_eq!(alice.s.decline_status, Some(200), "the first decline cancels the match");
    assert!(matches!(bob.s.decline_status, Some(200 | 409)), "the second finds it already cancelled");
    assert_eq!(alice.get_match()["status"], "cancelled");
    for u in [alice, bob] {
        let o = u.json("GET", &format!("/swap/offers/{}", u.s.offer_id), None);
        assert_eq!(o["status"], "open", "{}'s offer reopens", u.name);
    }
}

#[test]
fn htlc_spends_pass_fee_check_unrelated_feeless_tx_does_not() {
    let Some(w) = world(PRICE) else { return };
    let (mut alice, mut bob) = w.users(PRICE);
    negotiate(&mut alice, &mut bob, XBT_AMT, BTC_AMT);
    let t = alice.terms();
    drive(&mut alice, &[Step::Fund]);
    w.knots.mine(t.xbt_confs);
    drive(&mut bob, &[Step::Fund]);
    w.core.mine(t.btc_confs);

    // Unrelated payments without the service fee are refused on both chains.
    let to = |u: &User, c: Chain| TxOut { value: Amount::from_sat(100_000), script_pubkey: u.address(c, 3).script_pubkey() };
    let out = to(&bob, Chain::Xbt);
    let (_, s, body) = alice.pay(Chain::Xbt, vec![out], None);
    assert_eq!(s, 402, "a fee-less XBT payment: {body}");
    let out = to(&alice, Chain::Btc);
    let (_, s, body) = bob.pay(Chain::Btc, vec![out], None);
    assert_eq!(s, 402, "a fee-less BTC payment: {body}");

    // An HTLC refund is exempt from the fee check: before its locktime the
    // node refuses it (non-final), not the edge's fee check.
    drive(&mut alice, &[Step::Refund]);
    let (_, s, body) = alice.s.spend.clone().unwrap();
    assert!(s != 402 && s != 200, "an early XBT refund is non-final, not fee-rejected: {s} {body}");

    // Past each locktime, both refunds go through /<chain>/tx.
    w.core.set_mock_time(t.btc_locktime as u64 + 120);
    w.core.mine(12);
    drive(&mut bob, &[Step::Refund]);
    let (b_refund, s, body) = bob.s.spend.clone().unwrap();
    assert_eq!(s, 200, "bob's BTC refund through /btc/tx: {body}");
    assert_eq!(body.trim(), b_refund.compute_txid().to_string());

    w.knots.set_mock_time(t.xbt_locktime as u64 + 120);
    w.knots.mine(12);
    drive(&mut alice, &[Step::Refund]);
    let (a_refund, s, body) = alice.s.spend.clone().unwrap();
    assert_eq!(s, 200, "alice's XBT refund through /xbt/tx: {body}");
    assert_eq!(body.trim(), a_refund.compute_txid().to_string());
}

// ==========================================================================
// harness pieces, provable without the Knots node
// ==========================================================================

/// A regtest fee address for edges that run without nodes.
fn fee_address() -> String {
    let k = MasterKey::generate(&[0xfe; 32]).unwrap().1;
    let p = ChainParams::bitcoin_regtest();
    WalletView::new(p.clone(), k.account_xpub(&p, 0).unwrap()).address_at(0, 0).unwrap().to_string()
}

/// An edge with no reachable chain behind it: enough for quotes and the
/// negotiation routes.
fn chainless_edge(dir: &TempDir, stub: &StubNeoxex) -> Edge {
    Edge::start(dir, None, "http://127.0.0.1:9", stub.url(), &fee_address())
}

#[test]
fn harness_stub_neoxex_price_reaches_the_edge_quote() {
    let dir = TempDir::new("quote");
    let stub = StubNeoxex::start(PRICE);
    let edge = chainless_edge(&dir, &stub);
    let u = User::new("quoter", 1, &edge.url, PRICE);
    let q = u.json("GET", "/swap/quote", None);
    assert!(q["price_sat_per_xbt"].as_u64().unwrap().abs_diff(PRICE) <= 1, "{q}");
    assert_eq!(q["fee_bps"], 50);
    // Settable: the edge sees the new price once its 10 s cache expires.
    stub.set_price(PRICE * 2);
    poll("the new price", 20, || {
        let p = u.json("GET", "/swap/quote", None)["price_sat_per_xbt"].as_u64()?;
        (p.abs_diff(PRICE * 2) <= 1).then_some(())
    });
}

#[test]
fn harness_driver_negotiates_identical_contracts_over_http() {
    let dir = TempDir::new("negotiate");
    let stub = StubNeoxex::start(PRICE);
    let edge = chainless_edge(&dir, &stub);
    let mut alice = User::new("alice", 0xa1, &edge.url, PRICE);
    let mut bob = User::new("bob", 0xb0, &edge.url, PRICE);
    negotiate(&mut alice, &mut bob, XBT_AMT, BTC_AMT);
    assert_eq!(alice.s.match_id, bob.s.match_id);
    assert_eq!(alice.terms(), bob.terms());
    assert_eq!(alice.s.hashlock, bob.s.hashlock, "bob got alice's hashlock over the relay");
    assert_eq!(alice.s.their_keys, Some(bob.pubkeys()));
    assert_eq!(bob.s.their_keys, Some(alice.pubkeys()));
    // Both phones build byte-identical HTLCs for both legs.
    let (xbt, btc) = (alice.xbt.clone(), alice.btc.clone());
    let a_xbt = alice.machine().build_our_contract(&xbt).unwrap().script_pubkey.clone();
    assert_eq!(a_xbt, bob.machine().expected_their_contract(&xbt).script_pubkey);
    let b_btc = bob.machine().build_our_contract(&btc).unwrap().script_pubkey.clone();
    assert_eq!(b_btc, alice.machine().expected_their_contract(&btc).script_pubkey);
    let m = alice.get_match();
    assert_eq!(m["status"], "accepted");
}

#[test]
fn harness_driver_declines_a_skewed_price() {
    let dir = TempDir::new("decline");
    let stub = StubNeoxex::start(PRICE * 105 / 100);
    let edge = chainless_edge(&dir, &stub);
    let mut alice = User::new("alice", 0xa1, &edge.url, PRICE);
    let mut bob = User::new("bob", 0xb0, &edge.url, PRICE);
    drive(&mut alice, &[Step::Offer("xbt_to_btc", XBT_AMT)]);
    drive(&mut bob, &[Step::Offer("btc_to_xbt", btc_for_xbt(XBT_AMT, PRICE * 105 / 100))]);
    // Both see the match before either reviews it (a decline hides it from the offer).
    drive(&mut alice, &[Step::AwaitMatch]);
    drive(&mut bob, &[Step::AwaitMatch]);
    drive(&mut alice, &[Step::Review]);
    drive(&mut bob, &[Step::Review]);
    assert_declined_and_reopened(&alice, &bob);
}

#[test]
fn harness_esplora_shim_over_bitcoin_core() {
    let Ok(core_bin) = std::env::var("FORTIS_BTC_BITCOIND") else {
        eprintln!("skipping: set FORTIS_BTC_BITCOIND (Bitcoin Core) to test the Esplora shim");
        return;
    };
    let core = start_node(Path::new(&core_bin), Flavor::Core);
    core.mine(101);
    let shim = EsploraShim::start(core.rpc());
    let a = agent();
    let get = |path: &str| req(&a, "GET", &format!("{}{path}", shim.url()), &[], None);
    let json_of = |path: &str| -> Value {
        let (s, b) = get(path);
        assert_eq!(s, 200, "{path}: {b}");
        serde_json::from_str(&b).unwrap()
    };

    assert_eq!(get("/blocks/tip/height"), (200, "101".to_string()));
    assert!(json_of("/fee-estimates")["6"].as_f64().unwrap() >= 1.0);

    // A payment to a wallet-core address, still in the mempool.
    let mut bob = User::offline("bob", 0xb0, PRICE);
    let addr = bob.address(Chain::Btc, 0);
    let txid = core.wallet("miner", &["sendtoaddress", &addr.to_string(), "0.01"]);
    let v = json_of(&format!("/tx/{txid}"));
    assert_eq!(v["status"]["confirmed"], false);
    let n = v["vout"].as_array().unwrap().iter().position(|o| o["value"] == 1_000_000).unwrap();
    assert_eq!(v["vout"][n]["scriptpubkey"], hex::encode(addr.script_pubkey().as_bytes()));
    assert_eq!(get(&format!("/tx/{txid}/hex")), (200, core.cli(&["getrawtransaction", &txid])));
    assert_eq!(json_of(&format!("/tx/{txid}/outspend/{n}"))["spent"], false);

    core.mine(1);
    let v = json_of(&format!("/tx/{txid}"));
    assert_eq!(v["status"]["confirmed"], true);
    assert_eq!(v["status"]["block_height"], 102);

    // Spend it with wallet-core: POST /tx, then the outspend from the mempool.
    bob.coins.push((
        Chain::Btc,
        Utxo {
            outpoint: OutPoint::new(Txid::from_str(&txid).unwrap(), n as u32),
            value: Amount::from_sat(1_000_000),
            script_pubkey: addr.script_pubkey(),
            confirmations: 1,
            derivation_index: 0,
            is_change: false,
        },
    ));
    let out = TxOut { value: Amount::from_sat(300_000), script_pubkey: bob.address(Chain::Btc, 4).script_pubkey() };
    let (spend, spent) = bob.sign_payment(Chain::Btc, vec![out], None);
    let (s, body) = req(&a, "POST", &format!("{}/tx", shim.url()), &[], Some(&tx_hex(&spend)));
    assert_eq!((s, body.trim()), (200, spend.compute_txid().to_string().as_str()), "POST /tx");
    bob.spent(Chain::Btc, &spent);
    let os = json_of(&format!("/tx/{txid}/outspend/{n}"));
    assert_eq!(os["spent"], true);
    assert_eq!(os["txid"], spend.compute_txid().to_string());
    assert_eq!(os["vin"], 0);
    assert_eq!(os["status"]["confirmed"], false);

    // Mined, then buried: found by scanning blocks.
    core.mine(3);
    let os = json_of(&format!("/tx/{txid}/outspend/{n}"));
    assert_eq!(os["spent"], true);
    assert_eq!(os["txid"], spend.compute_txid().to_string());
    assert_eq!(os["status"]["confirmed"], true);
    assert_eq!(os["status"]["block_height"], 103);

    // Unknowns and garbage.
    let zero = "00".repeat(32);
    assert_eq!(get(&format!("/tx/{zero}")).0, 404);
    assert_eq!(get(&format!("/tx/{zero}/outspend/0")).0, 404);
    assert_eq!(req(&a, "POST", &format!("{}/tx", shim.url()), &[], Some("beef")).0, 400);

    // The edge proxies BTC reads to the shim and fee-checks broadcasts.
    let dir = TempDir::new("shim-edge");
    let stub = StubNeoxex::start(PRICE);
    let edge = Edge::start(&dir, None, shim.url(), stub.url(), &fee_address());
    let u = User::new("edge-user", 2, &edge.url, PRICE);
    let v = u.json("GET", &format!("/btc/tx/{}", spend.compute_txid()), None);
    assert_eq!(v["status"]["block_height"], 103);
    let tip = poll("the edge's BTC tip", 30, || {
        let (s, b) = u.http("GET", "/btc/blocks/tip/height", None);
        (s == 200).then_some(b)
    });
    assert_eq!(tip.trim(), "105");
    bob.edge = edge.url.clone();
    bob.token = u.token.clone();
    let change = spend.output.iter().position(|o| o.value.to_sat() != 300_000).unwrap() as u32;
    bob.coins.push((
        Chain::Btc,
        Utxo {
            outpoint: OutPoint::new(spend.compute_txid(), change),
            value: spend.output[change as usize].value,
            script_pubkey: spend.output[change as usize].script_pubkey.clone(),
            confirmations: 3,
            derivation_index: 0,
            is_change: true,
        },
    ));
    let out = TxOut { value: Amount::from_sat(100_000), script_pubkey: bob.address(Chain::Btc, 5).script_pubkey() };
    let (_, s, body) = bob.pay(Chain::Btc, vec![out], None);
    assert_eq!(s, 402, "the edge refuses a fee-less BTC payment: {body}");
}
