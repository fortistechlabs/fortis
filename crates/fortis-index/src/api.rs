//! The Esplora-shaped REST surface the fortis wallet's `EsploraBackend` calls:
//! `/blocks/tip/height`, `/address/:a/utxo`, `/address/:a/txs`,
//! `/v1/fees/recommended`, and `POST /tx` — plus `POST /scan`, which answers a
//! whole wallet's balance + history for many addresses in one call. Public
//! chain data only — no auth; bind to localhost or a trusted network, or front
//! it with a TLS/rate-limiting proxy.
//!
//! Every request reads one consistent chain state (`chainstate::view`) without
//! locks; all blocking work runs on tokio's blocking pool, so a slow node RPC
//! never holds up other requests.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Network, OutPoint, Txid};
use serde_json::{json, Value};
use tower::limit::GlobalConcurrencyLimitLayer;

use fortis_node::Rpc;

use crate::chainstate::{view, SharedChain, View};
use crate::db::{Db, TooHeavy, Unlisted};
use crate::keys::UtxoVal;
use crate::keys::{Program, TxNum};
use crate::mempool::{MempoolView, SharedMempool};
use crate::render::{esplora_tx, Renderer};
use crate::spends::{OutspendError, Spends};
use crate::sync::SyncStatus;

/// Most addresses one `POST /scan` may name.
pub const SCAN_MAX_ADDRESSES: usize = 1000;
pub const SCAN_DEFAULT_HISTORY: usize = 50;
pub const SCAN_MAX_HISTORY: usize = 100;
/// Confirmed transactions `GET /address/:a/txs` returns (newest first).
pub const TXS_HISTORY: usize = 100;
/// An address with more UTXO rows than this is refused as "too heavy".
pub const MAX_ROWS: usize = 10_000;
/// Pending transactions rendered per request (each may cost a node RPC).
pub const PENDING_MAX: usize = 100;

const MAX_CONCURRENT: usize = 64;
const BODY_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub chain: SharedChain,
    pub mempool: SharedMempool,
    pub renderer: Arc<Renderer>,
    pub rpc: Rpc,
    pub network: Network,
    pub status: Arc<SyncStatus>,
    pub chain_id: String,
    /// Single-output spend lookups (swap HTLCs) for the `/tx/:txid…` routes.
    pub spends: Arc<Spends>,
}

/// An error answered as `{"error": "<msg>"}` with its status.
#[derive(Debug)]
struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, msg: impl std::fmt::Display) -> Self {
        ApiError(status, msg.to_string())
    }

    fn bad_request(msg: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }

    /// Index errors: "too heavy" is the caller's problem (400), anything else
    /// ours — logged, never sent (it can name files).
    fn index(e: anyhow::Error) -> Self {
        if e.chain().any(|c| c.is::<TooHeavy>()) {
            return Self::bad_request("address too heavy");
        }
        eprintln!("fortis-index: index error: {e:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "the index could not answer")
    }

    /// Failures fetching detail from the node. The node being down is logged
    /// and answered generically (its text names the node's address); the
    /// node's own answer (an RPC error) is passed on.
    fn node(e: anyhow::Error) -> Self {
        if node_down(&e) {
            return Self::node_unavailable(&e);
        }
        Self::new(StatusCode::BAD_GATEWAY, format!("{e:#}"))
    }

    fn node_unavailable(e: &anyhow::Error) -> Self {
        eprintln!("fortis-index: node unavailable: {e:#}");
        Self::new(StatusCode::BAD_GATEWAY, "the node is unavailable")
    }
}

/// The node is down or unreachable (not an answer from it).
fn node_down(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<fortis_node::Unreachable>())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn text(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain")], body).into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/blocks/tip/height", get(tip_height))
        .route("/v1/fees/recommended", get(fees))
        .route("/tx", post(broadcast))
        .route("/scan", post(scan))
        .route("/txout/{txid}/{vout}", get(txout))
        .route("/tx/{txid}", get(tx_json))
        .route("/tx/{txid}/hex", get(tx_hex))
        .route("/tx/{txid}/outspend/{vout}", get(outspend))
        .route("/address/{addr}/utxo", get(address_utxo))
        .route("/address/{addr}/txs", get(address_txs))
        .fallback(no_route)
        .method_not_allowed_fallback(no_route)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT))
        .layer(middleware::from_fn(host_guard))
        .with_state(state)
}

/// Refuse a request addressed to any hostname but `localhost`. The index
/// listens on this machine for the edge, which (like local tools) uses an IP
/// address or `localhost`; DNS rebinding reaches 127.0.0.1 under a hostname a
/// web page controls, so that is refused. No CORS headers either: only the
/// edge talks to the index, and it adds its own for the public.
async fn host_guard(req: Request, next: Next) -> Response {
    let allowed = match req.headers().get(header::HOST) {
        None => true,
        Some(h) => h.to_str().is_ok_and(host_allowed),
    };
    if !allowed {
        return ApiError::new(StatusCode::FORBIDDEN, "address the index by IP address or localhost").into_response();
    }
    next.run(req).await
}

/// `localhost` or an IP literal, with or without a port.
fn host_allowed(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok()
}

async fn no_route() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no such route")
}

/// Run `f` on the blocking pool with a consistent view of the index.
async fn with_view<T: Send + 'static>(
    st: &AppState,
    f: impl FnOnce(&AppState, &View, &MempoolView) -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let st = st.clone();
    tokio::task::spawn_blocking(move || {
        let v = view(&st.db, &st.chain)
            .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
        let mp = st.mempool.load_full();
        f(&st, &v, &mp)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?
}

async fn root(State(st): State<AppState>) -> Json<Value> {
    let chain = st.chain.load();
    Json(json!({
        "name": "fortis-index",
        "version": env!("CARGO_PKG_VERSION"),
        "tip": chain.tip().map(|(h, _)| h),
        "node_tip": st.status.node_tip.load(Ordering::SeqCst),
        "mode": if st.status.follow.load(Ordering::SeqCst) { "follow" } else { "bulk" },
        "chain": st.chain_id,
        "mempool": st.mempool.load().len(),
    }))
}

async fn tip_height(State(st): State<AppState>) -> Response {
    let h = st.chain.load().tip().map_or(0, |(h, _)| h);
    text(StatusCode::OK, h.to_string())
}

async fn fees(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rpc = st.rpc.clone();
    let v = tokio::task::spawn_blocking(move || recommended_fees(&rpc))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(v))
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

async fn broadcast(State(st): State<AppState>, body: String) -> Response {
    let rpc = st.rpc.clone();
    match tokio::task::spawn_blocking(move || fortis_node::broadcast(&rpc, body.trim())).await {
        Ok(Ok(txid)) => text(StatusCode::OK, txid.to_string()),
        // The node down: generic (its text names the node). Its rejection
        // ("insufficient fee", …): passed on, the wallets act on it.
        Ok(Err(e)) if node_down(&e) => ApiError::node_unavailable(&e).into_response(),
        Ok(Err(e)) => text(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => {
            eprintln!("fortis-index: broadcast task failed: {e}");
            text(StatusCode::INTERNAL_SERVER_ERROR, "broadcast failed".into())
        }
    }
}

/// `GET /txout/{txid}/{vout}[?mempool=false]`: the node's `gettxout` for any
/// output type — the index only knows P2WPKH, so the swap HTLC checks (phones,
/// watchtower) ask the node directly. By default the mempool counts, so an
/// output spent by an unconfirmed tx reads as spent; `mempool=false` asks the
/// confirmed UTXO set only, so it stays until the spend confirms.
async fn txout(
    State(st): State<AppState>,
    Path((txid, vout)): Path<(String, u32)>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<Value>> {
    let txid: Txid = txid
        .parse()
        .map_err(|e| ApiError::bad_request(format!("bad txid: {e}")))?;
    let args = gettxout_args(&txid, vout, query.as_deref())?;
    let rpc = st.rpc.clone();
    let res = tokio::task::spawn_blocking(move || rpc.call("gettxout", args))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map_err(ApiError::node)?;
    txout_reply(&res)
}

fn parse_txid(s: &str) -> ApiResult<Txid> {
    s.parse()
        .map_err(|e| ApiError::bad_request(format!("bad txid: {e}")))
}

/// Run a node lookup off the request path.
async fn on_node<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?
}

fn unknown_tx() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "unknown transaction")
}

/// `GET /tx/:txid` — Esplora's transaction JSON, from the node (any
/// transaction, not only indexed addresses'): what swaps read an HTLC from.
async fn tx_json(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Json<Value>> {
    let txid = parse_txid(&txid)?;
    on_node(move || {
        let node = st.spends.node();
        let t = node.tx(&txid).map_err(ApiError::node)?.ok_or_else(unknown_tx)?;
        let block = match t["blockhash"].as_str().map(str::parse::<bitcoin::BlockHash>) {
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
        Ok(Json(esplora_tx(&t, status)))
    })
    .await
}

/// `GET /tx/:txid/hex` — the raw transaction.
async fn tx_hex(State(st): State<AppState>, Path(txid): Path<String>) -> ApiResult<Response> {
    let txid = parse_txid(&txid)?;
    on_node(move || {
        let hex = st.spends.node().raw_tx(&txid).map_err(ApiError::node)?.ok_or_else(unknown_tx)?;
        Ok(text(StatusCode::OK, hex))
    })
    .await
}

/// `GET /tx/:txid/outspend/:vout` — who spent the output, if anyone.
async fn outspend(
    State(st): State<AppState>,
    Path((txid, vout)): Path<(String, u32)>,
) -> ApiResult<Json<Value>> {
    let txid = parse_txid(&txid)?;
    on_node(move || match st.spends.outspend(&txid, vout) {
        Ok(o) => Ok(Json(o.json())),
        Err(OutspendError::UnknownOutput) => Err(ApiError::new(StatusCode::NOT_FOUND, "unknown output")),
        Err(OutspendError::Pending(m)) => Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, m)),
        Err(OutspendError::OutOfRange(m)) => Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, m)),
        Err(OutspendError::Node(e)) => Err(ApiError::node(e)),
    })
    .await
}

/// `gettxout`'s params for the `/txout` query string: `include_mempool` is
/// true unless `mempool=false`.
fn gettxout_args(txid: &Txid, vout: u32, query: Option<&str>) -> ApiResult<Value> {
    let mut include_mempool = true;
    for pair in query.unwrap_or("").split('&') {
        if let Some(v) = pair.strip_prefix("mempool=") {
            include_mempool = match v {
                "true" => true,
                "false" => false,
                _ => return Err(ApiError::bad_request("mempool must be true or false")),
            };
        }
    }
    Ok(json!([txid.to_string(), vout, include_mempool]))
}

/// Shape a `gettxout` result; `null` means spent or never existed.
fn txout_reply(res: &Value) -> ApiResult<Json<Value>> {
    if res.is_null() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "spent or unknown"));
    }
    Ok(Json(json!({
        "value_sat": crate::render::btc_to_sat(&res["value"]),
        "scriptpubkey_hex": res["scriptPubKey"]["hex"],
        "confirmations": res["confirmations"].as_u64().unwrap_or(0),
    })))
}

/// The P2WPKH program of `addr`, `None` for a valid address of another type
/// (never indexed, so it simply has no rows).
fn address_program(addr: &str, network: Network) -> ApiResult<Option<Program>> {
    let a = addr
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|e| ApiError::bad_request(format!("bad address {addr}: {e}")))?
        .require_network(network)
        .map_err(|_| {
            ApiError::bad_request(format!("address {addr} is not valid on this network"))
        })?;
    let spk = a.script_pubkey();
    Ok(spk
        .is_p2wpkh()
        .then(|| spk.as_bytes()[2..22].try_into().unwrap()))
}

/// Confirmed UTXOs (minus any spent by a pending tx), then unconfirmed ones
/// the mempool creates for `p`.
fn utxo_rows(v: &View, mp: &MempoolView, p: &Program) -> ApiResult<Vec<Value>> {
    let rows = v.reader.utxos(p, MAX_ROWS).map_err(ApiError::index)?;
    Ok(utxo_json(rows, mp, p))
}

/// The scan's coins for `p`: never "too heavy". Past [`MAX_ROWS`] the largest
/// are listed and the rest (dust, say) counted and summed, so a wallet dusted
/// by anyone who knows one of its addresses still shows its whole balance.
fn utxo_rows_capped(v: &View, mp: &MempoolView, p: &Program) -> ApiResult<(Vec<Value>, Unlisted)> {
    let (rows, rest) = v
        .reader
        .utxos_largest(p, MAX_ROWS, |op| !mp.is_spent(op))
        .map_err(ApiError::index)?;
    Ok((utxo_json(rows, mp, p), rest))
}

/// Confirmed `rows` not being spent in the mempool, then the mempool's own
/// unspent outputs for `p`, as Esplora `utxo` objects.
fn utxo_json(rows: Vec<(OutPoint, UtxoVal)>, mp: &MempoolView, p: &Program) -> Vec<Value> {
    let mut rows: Vec<Value> = rows
        .into_iter()
        .filter(|(op, _)| !mp.is_spent(op))
        .map(|(op, u)| {
            json!({
                "txid": op.txid.to_string(),
                "vout": op.vout,
                "value": u.value,
                "status": { "confirmed": true, "block_height": u.height },
            })
        })
        .collect();
    // A tx mined since the mempool snapshot is in both until the next refresh:
    // list its outputs once, as confirmed.
    let confirmed: HashSet<(String, u64)> = rows
        .iter()
        .map(|r| {
            (
                r["txid"].as_str().unwrap_or_default().to_string(),
                r["vout"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    for (op, value) in mp.utxos_for(p) {
        if confirmed.contains(&(op.txid.to_string(), op.vout as u64)) {
            continue;
        }
        rows.push(json!({
            "txid": op.txid.to_string(),
            "vout": op.vout,
            "value": value,
            "status": { "confirmed": false },
        }));
    }
    rows
}

async fn address_utxo(
    State(st): State<AppState>,
    Path(addr): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(p) = address_program(&addr, st.network)? else {
        return Ok(Json(json!([])));
    };
    with_view(&st, move |_, v, mp| {
        Ok(Json(Value::Array(utxo_rows(v, mp, &p)?)))
    })
    .await
}

/// Confirmed txnums of `rows` whose txid is not among `pending` (a block may
/// just have landed that the mempool snapshot still lists).
fn without_pending(v: &View, rows: Vec<TxNum>, pending: &HashSet<Txid>) -> ApiResult<Vec<TxNum>> {
    if pending.is_empty() {
        return Ok(rows);
    }
    let mut out = Vec::with_capacity(rows.len());
    for n in rows {
        let id = v.reader.txid(n).map_err(ApiError::index)?;
        if !id.is_some_and(|id| pending.contains(&id)) {
            out.push(n);
        }
    }
    Ok(out)
}

/// Mempool txs first, then confirmed newest-first, in the Esplora shape.
async fn address_txs(
    State(st): State<AppState>,
    Path(addr): Path<String>,
) -> ApiResult<Json<Value>> {
    let Some(p) = address_program(&addr, st.network)? else {
        return Ok(Json(json!([])));
    };
    with_view(&st, move |st, v, mp| {
        let ids = mp.txids_for(&p);
        let shown = &ids[..ids.len().min(PENDING_MAX)];
        let mut txs = st.renderer.pending(v, mp, shown).map_err(ApiError::node)?;
        let pending: HashSet<Txid> = ids.into_iter().collect();
        let rows = v.reader.history(&p, TXS_HISTORY).map_err(ApiError::index)?;
        let rows = without_pending(v, rows, &pending)?;
        txs.extend(st.renderer.confirmed(v, &rows).map_err(ApiError::node)?);
        Ok(Json(Value::Array(txs)))
    })
    .await
}

/// Validated `POST /scan` input: deduplicated `(address, program)` targets.
fn scan_targets(body: &str, network: Network) -> ApiResult<(Vec<(String, Program)>, usize)> {
    let req: Value = serde_json::from_str(body)
        .map_err(|e| ApiError::bad_request(format!("body must be JSON: {e}")))?;
    let Some(list) = req["addresses"].as_array() else {
        return Err(ApiError::bad_request("expected {\"addresses\": [...]}"));
    };
    if list.is_empty() || list.len() > SCAN_MAX_ADDRESSES {
        return Err(ApiError::bad_request(format!(
            "addresses: expected 1 to {SCAN_MAX_ADDRESSES}"
        )));
    }
    let history = req["history"]
        .as_u64()
        .map_or(SCAN_DEFAULT_HISTORY, |n| n as usize)
        .min(SCAN_MAX_HISTORY);
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(list.len());
    for v in list {
        let Some(addr) = v.as_str() else {
            return Err(ApiError::bad_request("addresses must be strings"));
        };
        if !seen.insert(addr) {
            continue;
        }
        // The index only stores P2WPKH — anything else would look "unused",
        // which is a wrong answer, not an empty one.
        match address_program(addr, network)? {
            Some(p) => targets.push((addr.to_string(), p)),
            None => {
                return Err(ApiError::bad_request(format!(
                    "address {addr}: only P2WPKH addresses are indexed"
                )))
            }
        }
    }
    Ok((targets, history))
}

/// Everything `POST /scan` can answer from the index and mempool alone.
struct ScanPlan {
    used: Vec<String>,
    utxos: Vec<Value>,
    /// `{address, count, value}` for each address past the row cap.
    unlisted: Vec<Value>,
    pending: Vec<Txid>,
    /// Newest first, each tx once, capped to the requested history.
    confirmed: Vec<TxNum>,
}

fn scan_plan(
    targets: &[(String, Program)],
    history: usize,
    v: &View,
    mp: &MempoolView,
) -> ApiResult<ScanPlan> {
    let mut used = Vec::new();
    let mut utxos = Vec::new();
    let mut unlisted = Vec::new();
    let mut pending = Vec::new();
    let mut pending_ids = HashSet::new();
    let mut confirmed = Vec::new();
    let mut confirmed_ids = HashSet::new();
    for (addr, p) in targets {
        let rows = v.reader.history(p, history).map_err(ApiError::index)?;
        let mem = mp.txids_for(p);
        if !rows.is_empty() || !mem.is_empty() {
            used.push(addr.clone());
        }
        let (rows_of_p, rest) = utxo_rows_capped(v, mp, p)?;
        for mut u in rows_of_p {
            u["address"] = json!(addr);
            utxos.push(u);
        }
        if rest.count > 0 {
            unlisted.push(json!({ "address": addr, "count": rest.count, "value": rest.value }));
        }
        for id in mem {
            if pending_ids.insert(id) {
                pending.push(id);
            }
        }
        for n in rows {
            if confirmed_ids.insert(n) {
                confirmed.push(n);
            }
        }
    }
    pending.truncate(PENDING_MAX);
    let mut confirmed = without_pending(v, confirmed, &pending_ids)?;
    // The newest `history` overall are within the newest `history` of each
    // address, so the per-address cap above lost nothing.
    confirmed.sort_unstable_by(|a, b| b.cmp(a));
    confirmed.truncate(history);
    Ok(ScanPlan {
        used,
        utxos,
        unlisted,
        pending,
        confirmed,
    })
}

/// `POST /scan` — body `{"addresses": [...], "history": 50}`. Everything a
/// wallet needs to show its balance and recent activity for a whole address
/// set at once:
///
/// ```text
/// { "tip":    972801,
///   "used":   ["bc1q…", …],            // appeared in any tx, pending included
///   "utxos":  [{ "address", "txid", "vout", "value", "status" }, …],
///   "txs":    [ <Esplora tx>, … ],     // pending first, then newest confirmed; each tx once
///   "unlisted": [{ "address", "count", "value" }, …], // past the per-address cap: the rest, summed
///   "failed": [] }                     // addresses that couldn't be checked
/// ```
///
/// `failed` is always empty here (one local database — all or nothing); it
/// exists so every backend serving `/scan` speaks one shape.
async fn scan(State(st): State<AppState>, body: String) -> ApiResult<Json<Value>> {
    let (targets, history) = scan_targets(&body, st.network)?;
    with_view(&st, move |st, v, mp| {
        let plan = scan_plan(&targets, history, v, mp)?;
        let mut txs = st
            .renderer
            .pending(v, mp, &plan.pending)
            .map_err(ApiError::node)?;
        txs.extend(
            st.renderer
                .confirmed(v, &plan.confirmed)
                .map_err(ApiError::node)?,
        );
        Ok(Json(json!({
            "tip": v.chain.tip().map_or(0, |(h, _)| h),
            "used": plan.used,
            "utxos": plan.utxos,
            "unlisted": plan.unlisted,
            "txs": txs,
            "failed": Vec::<String>::new(),
        })))
    })
    .await
}

#[cfg(test)]
mod tests;
