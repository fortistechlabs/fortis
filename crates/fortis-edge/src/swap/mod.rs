//! P2P BTC ⇄ XBT swaps: the `/swap/*` routes, owner-secret auth and config.
//!
//! Every route needs the install token (`handle` verifies it). `POST
//! /swap/offers` and `POST /swap/take/{code}` return an owner secret; every
//! later call about that offer or match carries it as `X-Swap-Secret`. The
//! store keeps only its SHA-256, and which hash matches decides the role.
//!
//! An offer posted with a `client_id` is idempotent: its owner secret is
//! HMAC-derived from (token, client_id), so a repeat POST — after a lost
//! response or a crash before the phone saved it — returns the same offer and
//! secret. Without one the secret is random.

pub mod matcher;
pub mod neoxex;
pub mod relay;
pub mod store;
pub mod watchtower;

use std::collections::HashSet;
use std::io::Read;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::hashes::{sha256, Hash};
use bitcoin::{Amount, OutPoint, PublicKey, ScriptBuf, Transaction, Txid};
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tiny_http::{Method, Request};
use wallet_core::swap_terms::{network_params, MIN_BTC_LEG_SAT};
use wallet_core::{Chain, HtlcContract};

use crate::{err, Reply};
use matcher::{btc_leg_value, xbt_for_btc};
use neoxex::NeoxEx;
use watchtower::ChainReader;
use store::{
    new_id, Direction, Match, MatchStatus, Offer, OfferStatus, Store, WatchJob, WatchKind,
};

pub const SLIPPAGE_CHOICES: [u32; 3] = [50, 100, 200];
pub const DEFAULT_EXPIRES_SECS: u64 = 24 * 3600;
const MAX_EXPIRES_SECS: u64 = 7 * 24 * 3600;
/// Missed funding deadlines in 24 h that bar a token from new offers.
pub const MAX_STRIKES: u32 = 3;
const LONG_POLL: Duration = Duration::from_secs(25);
/// Half the default 4 HTTP workers.
const DEFAULT_LONG_POLL_CAP: usize = 2;
/// Watch jobs carry a handful of signed transactions.
const MAX_BODY: u64 = 256 * 1024;
const MAX_VARIANTS: usize = 16;
/// Crockford base32: no I, L, O, U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Longest `client_id` (hex digits) an offer may carry.
const MAX_CLIENT_ID: usize = 64;

pub struct Swap {
    store: Arc<Store>,
    neoxex: NeoxEx,
    fee_bps: u32,
    max_btc_sat: u64,
    /// Terms network: "bitcoin" | "regtest".
    network: String,
    /// The service-fee address; both legs pay their commission to it.
    fee_address: String,
    /// How long `GET …/messages` waits for a message.
    long_poll: Duration,
    polls: PollGate,
    /// Reads HTLC outputs for a refund/redeem watch whose outpoint no
    /// `funded` message recorded; without one such a watch is refused.
    chain: Option<Arc<dyn ChainReader>>,
    /// The install-token HMAC key; derives owner secrets for offers posted
    /// with a `client_id`.
    owner_key: Vec<u8>,
}

/// Long-polls hold a blocking HTTP worker, so they are rationed: at most
/// `cap` at once, and one per (match, role). A poll over either limit
/// answers at once with whatever is already there.
struct PollGate {
    cap: usize,
    /// The (match, role) of every waiting poll; its size is the global count.
    inflight: Mutex<HashSet<(String, Role)>>,
}

/// A held long-poll slot; released on drop.
struct PollSlot<'a> {
    gate: &'a PollGate,
    key: (String, Role),
}

impl PollGate {
    fn new(cap: usize) -> Self {
        PollGate { cap: cap.max(1), inflight: Mutex::default() }
    }

    fn inflight(&self) -> MutexGuard<'_, HashSet<(String, Role)>> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn try_enter(&self, match_id: &str, role: Role) -> Option<PollSlot<'_>> {
        let key = (match_id.to_string(), role);
        let mut inflight = self.inflight();
        if inflight.len() >= self.cap || !inflight.insert(key.clone()) {
            return None;
        }
        Some(PollSlot { gate: self, key })
    }
}

impl Drop for PollSlot<'_> {
    fn drop(&mut self) {
        self.gate.inflight().remove(&self.key);
    }
}

impl Swap {
    pub fn new(
        store: Store,
        neoxex: NeoxEx,
        fee_bps: u32,
        max_btc_sat: u64,
        network: String,
        fee_address: String,
        owner_key: Vec<u8>,
    ) -> Self {
        Swap {
            store: Arc::new(store),
            neoxex,
            fee_bps,
            max_btc_sat,
            network,
            fee_address,
            long_poll: LONG_POLL,
            polls: PollGate::new(DEFAULT_LONG_POLL_CAP),
            chain: None,
            owner_key,
        }
    }

    /// The chain the watch route checks unrecorded HTLC outpoints against
    /// (the watchtower's own reader).
    pub fn with_chain_reader(mut self, r: Arc<dyn ChainReader>) -> Self {
        self.chain = Some(r);
        self
    }

    /// The swap store, shared with the watchtower thread.
    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    /// At most `cap` (min 1) long-polls wait at once; size it below the
    /// HTTP worker count so polls can't starve other traffic.
    pub fn with_long_poll_cap(mut self, cap: usize) -> Self {
        self.polls = PollGate::new(cap);
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Initiator,
    Participant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Initiator => "initiator",
            Role::Participant => "participant",
        }
    }

    /// The chain this role sends (and funds an HTLC) on.
    fn send_chain(self) -> Chain {
        match self {
            Role::Initiator => Chain::Xbt,
            Role::Participant => Chain::Btc,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn internal(e: impl std::fmt::Display) -> Reply {
    eprintln!("fortis-edge: swap route failed: {e}");
    err(500, "internal error")
}

pub(crate) fn parse_chain(s: &str) -> Option<Chain> {
    Chain::ALL.into_iter().find(|c| c.as_str() == s)
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

fn read_json<T: DeserializeOwned>(req: &mut Request) -> Result<T, Reply> {
    let mut body = Vec::new();
    let _ = req.as_reader().take(MAX_BODY).read_to_end(&mut body);
    let body = if body.iter().all(u8::is_ascii_whitespace) { b"{}".to_vec() } else { body };
    serde_json::from_slice(&body).map_err(|e| err(400, &format!("bad request body: {e}")))
}

fn query_param<'a>(req: &'a Request, key: &str) -> Option<&'a str> {
    let (_, q) = req.url().split_once('?')?;
    q.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

// ---- owner secrets ----

/// A fresh owner secret: (hex for the client, SHA-256 for the store).
fn new_secret() -> anyhow::Result<(String, [u8; 32])> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| anyhow::anyhow!("CSPRNG: {e}"))?;
    Ok((hex::encode(b), Sha256::digest(b).into()))
}

/// The owner secret of the offer `token` posts with `client_id`:
/// HMAC-SHA256(owner key, "swap-owner|" ‖ token ‖ "|" ‖ client_id), so a
/// replay gets the same one back. (hex for the client, SHA-256 for the store)
fn client_secret(sw: &Swap, token: &str, client_id: &str) -> (String, [u8; 32]) {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&sw.owner_key).expect("HMAC accepts any key length");
    mac.update(b"swap-owner|");
    mac.update(token.as_bytes());
    mac.update(b"|");
    mac.update(client_id.as_bytes());
    let b: [u8; 32] = mac.finalize().into_bytes().into();
    (hex::encode(b), Sha256::digest(b).into())
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// SHA-256 of the `X-Swap-Secret` header's 32 bytes; `None` if absent or malformed.
fn presented_hash(req: &Request) -> Option<[u8; 32]> {
    let h = req.headers().iter().find(|h| h.field.equiv("X-Swap-Secret"))?;
    let secret = hex32(h.value.as_str().trim())?;
    Some(Sha256::digest(secret).into())
}

fn owns_offer(req: &Request, o: &Offer) -> bool {
    presented_hash(req).is_some_and(|h| ct_eq(&h, &o.secret_hash))
}

fn role_in(req: &Request, m: &Match) -> Option<Role> {
    let h = presented_hash(req)?;
    // Evaluate both so timing doesn't say which side matched.
    let (i, p) = (ct_eq(&h, &m.initiator_secret_hash), ct_eq(&h, &m.participant_secret_hash));
    if i {
        Some(Role::Initiator)
    } else if p {
        Some(Role::Participant)
    } else {
        None
    }
}

fn share_code() -> anyhow::Result<String> {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).map_err(|e| anyhow::anyhow!("CSPRNG: {e}"))?;
    Ok(b.iter().map(|x| CROCKFORD[(x & 31) as usize] as char).collect())
}

/// Crockford decoding is case-insensitive and reads O as 0, I and L as 1.
fn normalize_code(code: &str) -> String {
    code.trim()
        .chars()
        .filter(|c| *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

// ---- routing ----

/// `/swap/*`. `path` has no query string; `token` is the verified install token.
pub fn route(req: &mut Request, method: &Method, path: &str, token: &str, sw: &Swap) -> Reply {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.first() != Some(&"swap") {
        return err(404, "no such route");
    }
    match (method, &parts[1..]) {
        (Method::Get, ["quote"]) => quote(sw),
        (Method::Post, ["offers"]) => create_offer(req, token, sw),
        (Method::Get, ["offers", id]) => get_offer(req, id, sw),
        (Method::Delete, ["offers", id]) => cancel_offer(req, id, sw),
        (Method::Post, ["take", code]) => take(req, code, token, sw),
        (Method::Get, ["matches", id]) => get_match(req, id, sw),
        (Method::Post, ["matches", id, "messages"]) => post_message(req, id, sw),
        (Method::Get, ["matches", id, "messages"]) => poll_messages(req, id, sw),
        (Method::Post, ["watch"]) => watch(req, sw),
        _ => err(404, "no such route"),
    }
}

fn price(sw: &Swap) -> Result<u64, Reply> {
    sw.neoxex.price().map_err(|e| err(503, &format!("{e:?}")))
}

fn quote(sw: &Swap) -> Reply {
    let price = match price(sw) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let now = unix_now();
    let open = |d: Direction| {
        sw.store
            .open_offers(d)
            .into_iter()
            .filter(|o| !o.private && o.expires_at > now)
            .count()
    };
    Reply::Json(
        200,
        json!({
            "price_sat_per_xbt": price,
            "fee_bps": sw.fee_bps,
            "min_btc_sat": MIN_BTC_LEG_SAT,
            "max_btc_sat": sw.max_btc_sat,
            "open": {
                "xbt_to_btc": open(Direction::XbtToBtc),
                "btc_to_xbt": open(Direction::BtcToXbt),
            },
        }),
    )
}

fn struck_out(sw: &Swap, token: &str) -> bool {
    sw.store.strikes(token, unix_now()) >= MAX_STRIKES
}

const STRUCK_OUT: &str = "too many missed funding deadlines — try again in 24 hours";

#[derive(Deserialize)]
struct NewOffer {
    direction: Direction,
    amount_sat: u64,
    max_slippage_bps: u32,
    expires_in: Option<u64>,
    #[serde(default)]
    private: bool,
    /// Idempotency key: hex, at most 64 digits.
    #[serde(default)]
    client_id: Option<String>,
}

fn create_offer(req: &mut Request, token: &str, sw: &Swap) -> Reply {
    let n: NewOffer = match read_json(req) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let client_id = n.client_id.as_deref().map(str::to_ascii_lowercase);
    if let Some(c) = &client_id {
        if c.is_empty() || c.len() > MAX_CLIENT_ID || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
            return err(400, "client_id must be 1..=64 hex digits");
        }
        // A replay adopts the offer as it now stands: no strike or price
        // check, and no matching.
        if let Some(o) = sw.store.offer_by_client(token, c) {
            return offer_created(sw, &o, &client_secret(sw, token, c).0);
        }
    }
    if struck_out(sw, token) {
        return err(429, STRUCK_OUT);
    }
    if !SLIPPAGE_CHOICES.contains(&n.max_slippage_bps) {
        return err(400, "max_slippage_bps must be 50, 100 or 200");
    }
    let expires_in = n.expires_in.unwrap_or(DEFAULT_EXPIRES_SECS);
    if expires_in == 0 || expires_in > MAX_EXPIRES_SECS {
        return err(400, "expires_in out of range");
    }
    let price = match price(sw) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let btc_leg = btc_leg_value(n.direction, n.amount_sat, price);
    if btc_leg < MIN_BTC_LEG_SAT || btc_leg > sw.max_btc_sat {
        return err(
            400,
            &format!("amount must be worth {MIN_BTC_LEG_SAT}..={} BTC sat", sw.max_btc_sat),
        );
    }

    let now = unix_now();
    // (offer, owner secret, newly made)
    let made = (|| -> anyhow::Result<(Offer, String, bool)> {
        // The lookup and the insert are one step, so racing replays make one offer.
        let _g = sw.store.lock_matching();
        let (secret, secret_hash) = match &client_id {
            Some(c) => {
                let derived = client_secret(sw, token, c);
                if let Some(o) = sw.store.offer_by_client(token, c) {
                    return Ok((o, derived.0, false));
                }
                derived
            }
            None => new_secret()?,
        };
        let share_code = if n.private { Some(unused_share_code(sw)?) } else { None };
        let offer = Offer {
            id: new_id()?,
            token: token.to_string(),
            secret_hash,
            direction: n.direction,
            amount_sat: n.amount_sat,
            max_slippage_bps: n.max_slippage_bps,
            created_at: now,
            expires_at: now + expires_in,
            private: n.private,
            share_code,
            status: OfferStatus::Open,
            match_id: None,
            client_id: client_id.clone(),
        };
        sw.store.put_offer(&offer)?;
        Ok((offer, secret, true))
    })();
    let (offer, secret, fresh) = match made {
        Ok(x) => x,
        Err(e) => return internal(e),
    };

    if fresh && !offer.private {
        // One match per call; keep going while a remainder is still open.
        loop {
            match matcher::match_offer(
                &sw.store,
                &offer,
                price,
                now,
                sw.max_btc_sat,
                &sw.network,
                &sw.fee_address,
            ) {
                Ok(Some(_)) => {
                    if sw.store.offer(&offer.id).is_none_or(|o| o.status != OfferStatus::Open) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("fortis-edge: matching offer {} failed: {e}", offer.id);
                    break;
                }
            }
        }
    }

    let current = sw.store.offer(&offer.id).unwrap_or(offer);
    offer_created(sw, &current, &secret)
}

/// The `POST /swap/offers` reply: the offer plus its owner secret.
fn offer_created(sw: &Swap, o: &Offer, secret: &str) -> Reply {
    let mut out = offer_json(sw, o);
    out["offer_id"] = json!(o.id);
    out["owner_secret"] = json!(secret);
    Reply::Json(200, out)
}

fn unused_share_code(sw: &Swap) -> anyhow::Result<String> {
    for _ in 0..8 {
        let code = share_code()?;
        if sw.store.offer_by_code(&code).is_none() {
            return Ok(code);
        }
    }
    anyhow::bail!("no free share code")
}

fn offer_matches(sw: &Swap, offer_id: &str) -> Vec<Match> {
    let mut v: Vec<Match> = sw
        .store
        .all_matches()
        .into_iter()
        .filter(|m| m.initiator_offer == offer_id || m.participant_offer == offer_id)
        .collect();
    v.sort_by_key(|m| m.terms.matched_at);
    v
}

/// `match_id` is the offer's final match, or else its latest live one (a
/// partially filled offer stays open and can be in several matches; all are
/// listed in `matches`).
fn offer_json(sw: &Swap, o: &Offer) -> Value {
    let matches = offer_matches(sw, &o.id);
    let match_id = o.match_id.clone().or_else(|| {
        matches.iter().rev().find(|m| m.status != MatchStatus::Cancelled).map(|m| m.id.clone())
    });
    json!({
        "offer_id": o.id,
        "direction": o.direction,
        "amount_sat": o.amount_sat,
        "max_slippage_bps": o.max_slippage_bps,
        "created_at": o.created_at,
        "expires_at": o.expires_at,
        "private": o.private,
        "share_code": o.share_code,
        "status": o.status,
        "match_id": match_id,
        "matches": matches
            .iter()
            .map(|m| json!({ "match_id": m.id, "status": m.status }))
            .collect::<Vec<_>>(),
    })
}

fn get_offer(req: &Request, id: &str, sw: &Swap) -> Reply {
    match sw.store.offer(id) {
        None => err(404, "no such offer"),
        Some(o) if !owns_offer(req, &o) => err(403, "wrong or missing X-Swap-Secret"),
        Some(o) => Reply::Json(200, offer_json(sw, &o)),
    }
}

fn cancel_offer(req: &Request, id: &str, sw: &Swap) -> Reply {
    let _g = sw.store.lock_matching();
    let Some(mut o) = sw.store.offer(id) else {
        return err(404, "no such offer");
    };
    if !owns_offer(req, &o) {
        return err(403, "wrong or missing X-Swap-Secret");
    }
    if o.status != OfferStatus::Open {
        return err(409, "only an unmatched offer can be cancelled");
    }
    o.status = OfferStatus::Cancelled;
    match sw.store.put_offer(&o) {
        Ok(()) => Reply::Json(200, offer_json(sw, &o)),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct TakeReq {
    max_slippage_bps: Option<u32>,
}

/// Takes the whole private offer behind `code` with a one-shot offer of the
/// opposite direction.
fn take(req: &mut Request, code: &str, token: &str, sw: &Swap) -> Reply {
    if struck_out(sw, token) {
        return err(429, STRUCK_OUT);
    }
    let t: TakeReq = match read_json(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let slippage = t.max_slippage_bps.unwrap_or(100);
    if !SLIPPAGE_CHOICES.contains(&slippage) {
        return err(400, "max_slippage_bps must be 50, 100 or 200");
    }
    let code = normalize_code(code);
    let Some(maker) = sw.store.offer_by_code(&code) else {
        return err(404, "unknown share code");
    };
    let price = match price(sw) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let direction = maker.direction.opposite();
    let amount_sat = match direction {
        Direction::XbtToBtc => xbt_for_btc(maker.amount_sat, price),
        Direction::BtcToXbt => btc_leg_value(maker.direction, maker.amount_sat, price),
    };
    let now = unix_now();
    let (secret, secret_hash) = match new_secret() {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let taker = Offer {
        id: match new_id() {
            Ok(id) => id,
            Err(e) => return internal(e),
        },
        token: token.to_string(),
        secret_hash,
        direction,
        amount_sat,
        max_slippage_bps: slippage,
        created_at: now,
        expires_at: now + DEFAULT_EXPIRES_SECS,
        // Never on the public book: it exists only for this take.
        private: true,
        share_code: None,
        status: OfferStatus::Open,
        match_id: None,
        client_id: None,
    };
    let m = match matcher::take_private(
        &sw.store,
        &code,
        &taker,
        price,
        now,
        sw.max_btc_sat,
        &sw.network,
        &sw.fee_address,
    ) {
        Ok(m) => m,
        Err(e) => return err(409, &e.to_string()),
    };

    // A cap-limited fill can leave the taker a remainder; it isn't an offer anyone asked for.
    {
        let _g = sw.store.lock_matching();
        if let Some(mut o) = sw.store.offer(&taker.id).filter(|o| o.status == OfferStatus::Open) {
            o.status = OfferStatus::Cancelled;
            if let Err(e) = sw.store.put_offer(&o) {
                return internal(e);
            }
        }
    }
    Reply::Json(200, json!({ "match_id": m.id, "owner_secret": secret, "offer_id": taker.id }))
}

fn outpoint_json(o: &Option<(String, u32)>) -> Value {
    match o {
        Some((txid, vout)) => json!({ "txid": txid, "vout": vout }),
        None => Value::Null,
    }
}

/// The match and the caller's role in it, or the error reply.
fn authed_match(req: &Request, id: &str, sw: &Swap) -> Result<(Match, Role), Reply> {
    let m = sw.store.get_match(id).ok_or_else(|| err(404, "no such match"))?;
    let role = role_in(req, &m).ok_or_else(|| err(403, "wrong or missing X-Swap-Secret"))?;
    Ok((m, role))
}

fn get_match(req: &Request, id: &str, sw: &Swap) -> Reply {
    let (m, role) = match authed_match(req, id, sw) {
        Ok(x) => x,
        Err(r) => return r,
    };
    Reply::Json(
        200,
        json!({
            "terms": m.terms,
            "role": role.as_str(),
            "status": m.status,
            "participant_fund_by": m.participant_fund_by,
            "xbt_funding": outpoint_json(&m.xbt_funding),
            "btc_funding": outpoint_json(&m.btc_funding),
        }),
    )
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Msg {
    Accept,
    Decline,
    Keys { xbt: String, btc: String },
    Hashlock { hex: String },
    Funded { chain: String, txid: String, vout: u32 },
}

/// A compressed public key, re-encoded as lowercase hex.
fn compressed_hex(s: &str) -> Option<String> {
    PublicKey::from_str(s).ok().filter(|k| k.compressed).map(|k| k.to_string())
}

fn terminal(s: MatchStatus) -> bool {
    matches!(s, MatchStatus::Cancelled | MatchStatus::Closed)
}

fn post_message(req: &mut Request, id: &str, sw: &Swap) -> Reply {
    let (_, role) = match authed_match(req, id, sw) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let body: Value = match read_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let msg: Msg = match serde_json::from_value(body.clone()) {
        Ok(msg) => msg,
        Err(e) => return err(400, &format!("bad message: {e}")),
    };
    let pushed = |body: Value| match relay::push(&sw.store, id, role.as_str(), body) {
        Ok(seq) => Reply::Json(200, json!({ "seq": seq })),
        Err(e) => internal(e),
    };

    match msg {
        Msg::Decline => match matcher::on_decline(&sw.store, id) {
            Ok(()) => pushed(body),
            Err(e) => err(409, &e.to_string()),
        },
        Msg::Accept => {
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            match m.status {
                MatchStatus::Proposed => {}
                MatchStatus::Accepted => return pushed(body),
                _ => return err(409, "match is past acceptance"),
            }
            if unix_now() > m.terms.accept_by as u64 {
                return err(409, "accept deadline passed");
            }
            match role {
                Role::Initiator => m.accepted.0 = true,
                Role::Participant => m.accepted.1 = true,
            }
            if m.accepted == (true, true) {
                m.status = MatchStatus::Accepted;
            }
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(body)
        }
        // Keys and hashlock fix the HTLC scripts the fee exemption checks
        // spends against, so each is set once: an identical repeat is a
        // no-op, a different one a conflict.
        Msg::Keys { xbt, btc } => {
            let (Some(xbt), Some(btc)) = (compressed_hex(&xbt), compressed_hex(&btc)) else {
                return err(400, "keys must be compressed public keys");
            };
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            if terminal(m.status) {
                return err(409, "match is over");
            }
            let slot = match role {
                Role::Initiator => &mut m.keys.0,
                Role::Participant => &mut m.keys.1,
            };
            match slot {
                Some(existing) if *existing == (xbt.clone(), btc.clone()) => {
                    return Reply::Json(200, json!({ "seq": Value::Null }))
                }
                Some(_) => return err(409, "different keys are already recorded"),
                None => *slot = Some((xbt, btc)),
            }
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(body)
        }
        Msg::Hashlock { hex } => {
            if role != Role::Initiator {
                return err(403, "only the initiator sends the hashlock");
            }
            let Some(h) = hex32(&hex) else {
                return err(400, "hashlock must be 32 bytes of hex");
            };
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            if terminal(m.status) {
                return err(409, "match is over");
            }
            let h = hex::encode(h);
            match &m.hashlock {
                Some(existing) if *existing == h => {
                    return Reply::Json(200, json!({ "seq": Value::Null }))
                }
                Some(_) => return err(409, "a different hashlock is already recorded"),
                None => m.hashlock = Some(h),
            }
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(body)
        }
        Msg::Funded { chain, txid, vout } => {
            if parse_chain(&chain) != Some(role.send_chain()) {
                return err(400, &format!("the {} funds on {}", role.as_str(), role.send_chain().as_str()));
            }
            let Ok(txid) = Txid::from_str(&txid) else {
                return err(400, "bad txid");
            };
            let outpoint = (txid.to_string(), vout);
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            let (slot, needs, next) = match role {
                Role::Initiator => {
                    (&mut m.xbt_funding, MatchStatus::Accepted, MatchStatus::InitiatorFunded)
                }
                Role::Participant => {
                    (&mut m.btc_funding, MatchStatus::InitiatorFunded, MatchStatus::ParticipantFunded)
                }
            };
            // A refund watch may have recorded this funding first (see
            // `record_leg_funding`); `funded` then still moves the match on.
            match slot {
                Some(existing) if *existing == outpoint && m.status != needs => {
                    return Reply::Json(200, json!({ "seq": Value::Null }))
                }
                Some(existing) if *existing != outpoint => {
                    return err(409, "a different funding is already recorded")
                }
                _ => {}
            }
            if m.status != needs {
                return err(409, "not your turn to fund");
            }
            *slot = Some(outpoint);
            m.status = next;
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(body)
        }
    }
}

fn poll_messages(req: &Request, id: &str, sw: &Swap) -> Reply {
    let role = match authed_match(req, id, sw) {
        Ok((_, role)) => role,
        Err(r) => return r,
    };
    let after = match query_param(req, "after").map(str::parse::<u64>) {
        None => 0,
        Some(Ok(n)) => n,
        Some(Err(_)) => return err(400, "bad after"),
    };
    // Over the long-poll limits, answer at once instead of holding a worker.
    let msgs = match sw.polls.try_enter(id, role) {
        Some(_slot) => relay::wait(&sw.store, id, after, sw.long_poll),
        None => sw.store.msgs_after(id, after),
    };
    Reply::Json(
        200,
        json!({
            "messages": msgs
                .into_iter()
                .map(|(seq, from, body)| json!({ "seq": seq, "from": from, "body": body }))
                .collect::<Vec<_>>(),
        }),
    )
}

#[derive(Deserialize)]
struct OutpointReq {
    txid: String,
    vout: u32,
}

#[derive(Deserialize)]
struct WatchReq {
    match_id: String,
    chain: String,
    /// "refund" | "redeem"
    kind: String,
    htlc_outpoint: OutpointReq,
    not_before: u32,
    variants: Vec<String>,
    /// For a preimage-less redeem: all three, hex; `sigs[i]` signs `variants[i]`.
    sigs: Option<Vec<String>>,
    pubkey: Option<String>,
    witness_script: Option<String>,
}

/// Hands the watchtower pre-signed spends of one of the match's recorded
/// HTLC outpoints: a refund of the caller's own HTLC, or a redeem of the
/// counterparty's.
fn watch(req: &mut Request, sw: &Swap) -> Reply {
    let w: WatchReq = match read_json(req) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let (m, role) = match authed_match(req, &w.match_id, sw) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(chain) = parse_chain(&w.chain) else {
        return err(400, "chain must be xbt or btc");
    };
    let is_hex = |h: &String| !h.is_empty() && hex::decode(h).is_ok();
    let kind = match (w.kind.as_str(), w.sigs, w.pubkey, w.witness_script) {
        ("refund", None, None, None) => WatchKind::Refund,
        ("redeem", None, None, None) => WatchKind::Redeem,
        ("redeem", Some(sigs_hex), Some(pubkey_hex), Some(witness_script_hex))
            if is_hex(&pubkey_hex) && is_hex(&witness_script_hex) && sigs_hex.iter().all(is_hex) =>
        {
            if sigs_hex.len() != w.variants.len() {
                return err(400, "sigs must hold one signature per variant, in order");
            }
            WatchKind::WaitingRedeem { pubkey_hex, witness_script_hex, sigs_hex }
        }
        _ => return err(400, "kind must be refund or redeem (with sigs, pubkey and witness_script hex for a preimage-less redeem)"),
    };
    let own = chain == role.send_chain();
    if own != (kind == WatchKind::Refund) {
        return err(403, "refund your own HTLC, redeem the counterparty's");
    }

    let Ok(txid) = Txid::from_str(&w.htlc_outpoint.txid) else {
        return err(400, "bad htlc_outpoint txid");
    };
    let htlc = OutPoint { txid, vout: w.htlc_outpoint.vout };
    if w.variants.is_empty() || w.variants.len() > MAX_VARIANTS {
        return err(400, "1 to 16 variants");
    }
    let spends_htlc = |hex_tx: &String| {
        hex::decode(hex_tx)
            .ok()
            .and_then(|b| bitcoin::consensus::deserialize::<Transaction>(&b).ok())
            .is_some_and(|tx| tx.input.len() == 1 && tx.input[0].previous_output == htlc)
    };
    if !w.variants.iter().all(spends_htlc) {
        return err(400, "every variant must be a transaction spending only htlc_outpoint");
    }
    if let Err(r) = record_leg_funding(sw, &m, chain, &htlc, kind == WatchKind::Refund) {
        return r;
    }

    let job = WatchJob {
        match_id: m.id,
        chain,
        kind,
        htlc: (txid.to_string(), htlc.vout),
        not_before: w.not_before,
        variants: w.variants,
        done: false,
        sent_prevout: None,
    };
    match sw.store.put_watch(&job) {
        Ok(()) => Reply::Json(200, json!({ "ok": true })),
        Err(e) => internal(e),
    }
}

/// Makes `htlc` the match's funding on `chain`, or refuses (409) the watch.
///
/// The recorded funding is accepted as is. With none recorded and `by_owner`
/// (a refund watch by the leg's owner; anything else gets 409) (the phone's
/// `funded` was refused or hasn't landed: say the participant declined
/// between our broadcast and our `funded`, or a late funding of a match that
/// was cancelled meanwhile), `htlc` is accepted only if the chain shows it
/// pays exactly this leg's HTLC script (from both relayed keys and the
/// hashlock) and exactly the leg amount, whatever the match status. It is
/// then recorded as the leg's funding, so the owner's own refund through
/// `/<chain>/tx` is fee-exempt too; the status is left alone. The chain is
/// read without `lock_matching()`; the write re-reads under it.
fn record_leg_funding(sw: &Swap, m: &Match, chain: Chain, htlc: &OutPoint, by_owner: bool) -> Result<(), Reply> {
    let want = (htlc.txid.to_string(), htlc.vout);
    let slot = |m: &Match| match chain {
        Chain::Xbt => m.xbt_funding.clone(),
        Chain::Btc => m.btc_funding.clone(),
    };
    match slot(m) {
        Some(f) if f == want => return Ok(()),
        Some(_) => return Err(err(409, "htlc_outpoint is not this match's recorded funding on that chain")),
        None => {}
    }
    // Only the leg's owner (a refund watch) may record it from the chain: a
    // redeem watch names the counterparty's leg, where an unconfirmed
    // look-alike could be double-spent later and poison the real funding.
    if !by_owner {
        return Err(err(409, "htlc_outpoint is not this match's recorded funding on that chain"));
    }
    let unknown = || err(409, "htlc_outpoint is not this match's HTLC on that chain");
    let Some(ws) = leg_witness_script(m, chain) else {
        return Err(err(409, "htlc_outpoint is not recorded, and both keys and the hashlock are needed to check it"));
    };
    let Some(reader) = &sw.chain else {
        return Err(unknown());
    };
    let out = match reader.txout(chain, &want.0, want.1) {
        Ok(Some(out)) => out,
        Ok(None) => return Err(unknown()),
        Err(e) => return Err(err(502, &format!("chain read failed: {e:#}"))),
    };
    let spk = ScriptBuf::new_p2wsh(&ws.wscript_hash());
    let amount = match chain {
        Chain::Xbt => m.terms.xbt_sat,
        Chain::Btc => m.terms.btc_sat,
    };
    if !out.scriptpubkey_hex.eq_ignore_ascii_case(&hex::encode(spk.as_bytes())) || out.value_sat != amount {
        return Err(unknown());
    }

    let _g = sw.store.lock_matching();
    let Some(mut cur) = sw.store.get_match(&m.id) else {
        return Err(err(404, "no such match"));
    };
    // Keys and hashlock are set-once, so the script checked above still holds.
    let cur_slot = match chain {
        Chain::Xbt => &mut cur.xbt_funding,
        Chain::Btc => &mut cur.btc_funding,
    };
    match cur_slot {
        Some(f) if *f == want => return Ok(()),
        Some(_) => return Err(err(409, "a different funding was recorded meanwhile")),
        None => *cur_slot = Some(want),
    }
    sw.store.put_match(&cur).map_err(internal)
}

/// The witness script of `m`'s HTLC on `chain`, built from the relayed keys
/// and hashlock; `None` until all of them are known. The XBT leg is the
/// initiator's (the participant redeems, the initiator refunds), the BTC leg
/// the participant's (the initiator redeems, the participant refunds).
fn leg_witness_script(m: &Match, chain: Chain) -> Option<ScriptBuf> {
    let key = |hex: &str| PublicKey::from_str(hex).ok().map(|k| k.inner);
    let (Some((i_xbt, i_btc)), Some((p_xbt, p_btc))) = (&m.keys.0, &m.keys.1) else {
        return None;
    };
    let hashlock = sha256::Hash::from_byte_array(hex32(m.hashlock.as_deref()?)?);
    let (btc_params, xbt_params) = network_params(&m.terms.network).ok()?;
    let c = match chain {
        Chain::Xbt => HtlcContract::build(
            &xbt_params,
            hashlock,
            &key(p_xbt)?,
            &key(i_xbt)?,
            m.terms.xbt_locktime,
            Amount::from_sat(m.terms.xbt_sat),
        ),
        Chain::Btc => HtlcContract::build(
            &btc_params,
            hashlock,
            &key(i_btc)?,
            &key(p_btc)?,
            m.terms.btc_locktime,
            Amount::from_sat(m.terms.btc_sat),
        ),
    };
    Some(c.witness_script)
}

/// True only if `raw_tx_hex` decodes and **every** input spends an outpoint
/// recorded as a match's funding on `chain` (the chain it is broadcast to)
/// with that match's HTLC witness script as its last witness element. Such a
/// spend is exempt from the service-fee check: its commission was paid when
/// the HTLC was funded. P2WSH validity then guarantees the prevout really is
/// that HTLC, so any other coin declared as "funding" earns nothing.
pub fn is_swap_spend(sw: &Swap, chain: Chain, raw_tx_hex: &str) -> bool {
    let Ok(bytes) = hex::decode(raw_tx_hex.trim()) else {
        return false;
    };
    let Ok(tx) = bitcoin::consensus::deserialize::<Transaction>(&bytes) else {
        return false;
    };
    !tx.input.is_empty()
        && tx.input.iter().all(|i| {
            let o = i.previous_output;
            // Several matches may claim one outpoint (a squatter can name a
            // victim's HTLC); the spend counts if it matches any of them.
            sw.store
                .matches_funded_by(chain, &o.txid.to_string(), o.vout)
                .iter()
                .filter_map(|m| leg_witness_script(m, chain))
                .any(|ws| i.witness.last() == Some(ws.as_bytes()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use bitcoin::{absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence};
    use bitcoin::{Transaction, TxIn, TxOut, Txid, Witness};
    use serde_json::{json, Value};

    use bitcoin::hashes::{sha256, Hash};
    use wallet_core::{network_params, HtlcContract, SwapKey, SwapTerms};

    use super::neoxex::NeoxEx;
    use super::store::Store;

    const BOOK: &str = include_str!("../../tests/fixtures/neoxex/orderbook_ok.json");
    const TICKER: &str = include_str!("../../tests/fixtures/neoxex/ticker_ok.json");
    const PAIRS: &str = include_str!("../../tests/fixtures/neoxex/pairs_live.json");
    /// The ticker fixture's `computedAt`, ms.
    const COMPUTED_MS: &str = "1791670572294";
    /// `weighted_mid_of_fixture` in neoxex.rs.
    const PRICE: u64 = 1_008_980;
    /// secp256k1 generator, compressed — any valid pubkey will do.
    const PUBKEY: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn temp_dir(label: &str) -> crate::test_tmp::TempDir {
        crate::test_tmp::TempDir::new(&format!("swap-routes-test-{label}"))
    }

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// Serves the NeoxEX fixtures. `fresh` rewrites the ticker's `computedAt`
    /// to now at serve time; otherwise the fixture's old timestamp makes it stale.
    fn neoxex_stub(fresh: bool) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let body = if url.ends_with("/orderbook/BTCB2_BTC") {
                    BOOK.to_string()
                } else if url.ends_with("/ticker/BTCB2_BTC") {
                    if fresh {
                        TICKER.replace(COMPUTED_MS, &unix_ms().to_string())
                    } else {
                        TICKER.to_string()
                    }
                } else if url.ends_with("/pairs") {
                    PAIRS.to_string()
                } else {
                    let _ = req.respond(tiny_http::Response::empty(404));
                    continue;
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        url
    }

    /// A swap service over a fresh store; the store's dir goes when the
    /// returned guard does (an [`Edge`] keeps it for the test's length).
    fn new_swap(fresh: bool) -> (Swap, crate::test_tmp::TempDir) {
        let dir = temp_dir("db");
        let sw = Swap::new(
            Store::open(&dir).unwrap(),
            NeoxEx::new(neoxex_stub(fresh)),
            50,
            1_000_000,
            "bitcoin".into(),
            "bc1qfee".into(),
            b"a-test-token-key-32-bytes-long!!".to_vec(),
        );
        (sw, dir)
    }

    /// Serves `route` over HTTP; the bearer token is passed through unverified
    /// (`handle` verifies it before routing).
    struct Edge {
        url: String,
        sw: Arc<Swap>,
        _db: crate::test_tmp::TempDir,
    }

    fn edge_with((sw, db): (Swap, crate::test_tmp::TempDir)) -> Edge {
        let sw = Arc::new(sw);
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        for _ in 0..4 {
            let (server, sw) = (Arc::clone(&server), Arc::clone(&sw));
            thread::spawn(move || {
                for mut req in server.incoming_requests() {
                    let method = req.method().clone();
                    let full = req.url().to_string();
                    let path = full.split('?').next().unwrap_or("").to_string();
                    let token = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string))
                        .unwrap_or_default();
                    let reply = route(&mut req, &method, &path, &token, &sw);
                    let _ = crate::respond(req, reply, "*");
                }
            });
        }
        Edge { url, sw, _db: db }
    }

    fn edge() -> Edge {
        edge_with(new_swap(true))
    }

    fn call(
        method: &str,
        url: &str,
        token: &str,
        secret: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = ureq::request(method, url).set("Authorization", &format!("Bearer {token}"));
        if let Some(s) = secret {
            r = r.set("X-Swap-Secret", s);
        }
        let res = match body {
            Some(b) => r.send_json(b),
            None => r.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("{method} {url}: {e}"),
        };
        let status = resp.status();
        (status, resp.into_json().unwrap_or(Value::Null))
    }

    impl Edge {
        fn get(&self, path: &str, token: &str, secret: Option<&str>) -> (u16, Value) {
            call("GET", &format!("{}{path}", self.url), token, secret, None)
        }
        fn post(&self, path: &str, token: &str, secret: Option<&str>, body: Value) -> (u16, Value) {
            call("POST", &format!("{}{path}", self.url), token, secret, Some(body))
        }
        fn delete(&self, path: &str, token: &str, secret: Option<&str>) -> (u16, Value) {
            call("DELETE", &format!("{}{path}", self.url), token, secret, None)
        }

        fn offer(&self, token: &str, direction: &str, amount_sat: u64, private: bool) -> (u16, Value) {
            self.post(
                "/swap/offers",
                token,
                None,
                json!({ "direction": direction, "amount_sat": amount_sat,
                        "max_slippage_bps": 100, "private": private }),
            )
        }

        /// A 200 k sat BTC offer (tok-a) matched by a 20 M sat XBT offer (tok-b).
        /// Returns (match id, initiator secret, participant secret).
        fn matched(&self) -> (String, String, String) {
            let (s, a) = self.offer("tok-a", "btc_to_xbt", 200_000, false);
            assert_eq!(s, 200, "{a}");
            assert!(a["match_id"].is_null());
            let (s, b) = self.offer("tok-b", "xbt_to_btc", 20_000_000, false);
            assert_eq!(s, 200, "{b}");
            let match_id = b["match_id"].as_str().expect("matched on post").to_string();
            let a_secret = a["owner_secret"].as_str().unwrap();
            let (s, a_now) =
                self.get(&format!("/swap/offers/{}", a["offer_id"].as_str().unwrap()), "tok-a", Some(a_secret));
            assert_eq!(s, 200, "{a_now}");
            assert_eq!(a_now["match_id"], match_id.as_str());
            (match_id, b["owner_secret"].as_str().unwrap().into(), a_secret.into())
        }

        fn msg(&self, match_id: &str, secret: &str, body: Value) -> (u16, Value) {
            self.post(&format!("/swap/matches/{match_id}/messages"), "tok", Some(secret), body)
        }
    }

    #[test]
    fn quote_returns_price_and_limits() {
        let e = edge();
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 200, "{q}");
        assert_eq!(q["price_sat_per_xbt"], PRICE);
        assert_eq!(q["fee_bps"], 50);
        assert_eq!(q["min_btc_sat"], 80_000);
        assert_eq!(q["max_btc_sat"], 1_000_000);
        assert_eq!(q["open"], json!({ "xbt_to_btc": 0, "btc_to_xbt": 0 }));

        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, false).0, 200);
        // Private offers aren't on the public book.
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, true).0, 200);
        let (_, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(q["open"], json!({ "xbt_to_btc": 0, "btc_to_xbt": 1 }));
    }

    #[test]
    fn quote_503_when_neoxex_refuses() {
        let e = edge_with(new_swap(false));
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 503, "{q}");
        assert_eq!(q["error"], "Stale");
        // No price, no offers either.
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, false).0, 503);
    }

    #[test]
    fn offer_rejects_bad_slippage_and_out_of_range_amounts() {
        let e = edge();
        let bad_slip = json!({ "direction": "btc_to_xbt", "amount_sat": 200_000, "max_slippage_bps": 75 });
        assert_eq!(e.post("/swap/offers", "tok-a", None, bad_slip).0, 400);
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 79_999, false).0, 400);
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 1_000_001, false).0, 400);
        // XBT amounts are judged by their BTC-leg value at the quote:
        // 7 M XBT sat ≈ 70 628 BTC sat; 100 M ≈ 1 008 980.
        assert_eq!(e.offer("tok-a", "xbt_to_btc", 7_000_000, false).0, 400);
        assert_eq!(e.offer("tok-a", "xbt_to_btc", 100_000_000, false).0, 400);
        let junk = json!({ "direction": "sideways", "amount_sat": 200_000, "max_slippage_bps": 100 });
        assert_eq!(e.post("/swap/offers", "tok-a", None, junk).0, 400);

        for slip in [50, 100, 200] {
            let ok = json!({ "direction": "btc_to_xbt", "amount_sat": 80_000, "max_slippage_bps": slip });
            let (s, o) = e.post("/swap/offers", "tok-a", None, ok);
            assert_eq!(s, 200, "{o}");
            assert_eq!(o["owner_secret"].as_str().unwrap().len(), 64);
            assert!(o["share_code"].is_null());
        }
        let (s, o) = e.offer("tok-a", "xbt_to_btc", 99_000_000, false);
        assert_eq!(s, 200, "{o}");
    }

    #[test]
    fn match_routes_need_the_owner_secret() {
        let e = edge();
        let (id, init, part) = e.matched();
        let path = format!("/swap/matches/{id}");
        let wrong = "ab".repeat(32);
        for secret in [None, Some(wrong.as_str()), Some("not-hex")] {
            assert_eq!(e.get(&path, "tok-a", secret).0, 403);
            assert_eq!(e.get(&format!("{path}/messages"), "tok-a", secret).0, 403);
            assert_eq!(e.post(&format!("{path}/messages"), "tok-a", secret, json!({"type": "accept"})).0, 403);
        }

        let (s, m) = e.get(&path, "tok-b", Some(&init));
        assert_eq!(s, 200, "{m}");
        assert_eq!(m["role"], "initiator");
        assert_eq!(m["status"], "proposed");
        assert_eq!(m["terms"]["match_id"], id.as_str());
        assert_eq!(m["terms"]["price_sat_per_xbt"], PRICE);
        assert_eq!(m["terms"]["fee_address"], "bc1qfee");
        assert!(m["participant_fund_by"].is_null());
        assert!(m["xbt_funding"].is_null() && m["btc_funding"].is_null());
        assert_eq!(e.get(&path, "tok-a", Some(&part)).1["role"], "participant");

        // Offers too.
        let (_, o) = e.offer("tok-c", "btc_to_xbt", 90_000, false);
        let opath = format!("/swap/offers/{}", o["offer_id"].as_str().unwrap());
        assert_eq!(e.get(&opath, "tok-c", None).0, 403);
        assert_eq!(e.get(&opath, "tok-c", Some(&init)).0, 403);
        assert_eq!(e.get(&opath, "tok-c", o["owner_secret"].as_str()).1["status"], "open");
    }

    #[test]
    fn cancel_only_while_unmatched() {
        let e = edge();
        let (_, o) = e.offer("tok-c", "btc_to_xbt", 90_000, false);
        let secret = o["owner_secret"].as_str().unwrap();
        let path = format!("/swap/offers/{}", o["offer_id"].as_str().unwrap());
        assert_eq!(e.delete(&path, "tok-c", Some(&"ab".repeat(32))).0, 403);
        assert_eq!(e.delete(&path, "tok-c", Some(secret)).0, 200);
        assert_eq!(e.get(&path, "tok-c", Some(secret)).1["status"], "cancelled");
        // Cancelled once; not again.
        assert_eq!(e.delete(&path, "tok-c", Some(secret)).0, 409);

        let (_, b) = e.offer("tok-b", "xbt_to_btc", 20_000_000, false);
        let (_, a) = e.offer("tok-a", "btc_to_xbt", 200_000, false);
        assert!(a["match_id"].is_string(), "{a}");
        for o in [&a, &b] {
            let p = format!("/swap/offers/{}", o["offer_id"].as_str().unwrap());
            assert_eq!(e.delete(&p, "tok", o["owner_secret"].as_str()).0, 409);
        }
    }

    #[test]
    fn take_by_code_returns_match_and_secret() {
        let e = edge();
        let (s, p) = e.offer("tok-a", "btc_to_xbt", 200_000, true);
        assert_eq!(s, 200, "{p}");
        let code = p["share_code"].as_str().unwrap().to_string();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c)), "{code}");
        // A public opposite offer doesn't take a private one.
        assert!(e.offer("tok-b", "xbt_to_btc", 20_000_000, false).1["match_id"].is_null());

        assert_eq!(e.post("/swap/take/ZZZZZZZZ", "tok-c", None, json!({})).0, 404);
        assert_eq!(e.post(&format!("/swap/take/{code}"), "tok-a", None, json!({})).0, 409);
        let (s, t) = e.post(&format!("/swap/take/{}", code.to_lowercase()), "tok-c", None, json!({}));
        assert_eq!(s, 200, "{t}");
        let id = t["match_id"].as_str().unwrap();
        let (s, m) = e.get(&format!("/swap/matches/{id}"), "tok-c", t["owner_secret"].as_str());
        assert_eq!(s, 200, "{m}");
        assert_eq!(m["role"], "initiator");
        let btc = m["terms"]["btc_sat"].as_u64().unwrap();
        assert!((199_990..=200_000).contains(&btc), "{btc}");
        let maker = format!("/swap/offers/{}", p["offer_id"].as_str().unwrap());
        assert_eq!(e.get(&maker, "tok-a", p["owner_secret"].as_str()).1["match_id"], id);
        // Taken once.
        assert_eq!(e.post(&format!("/swap/take/{code}"), "tok-d", None, json!({})).0, 409);
    }

    #[test]
    fn both_accept_moves_match_to_accepted() {
        let e = edge();
        let (id, init, part) = e.matched();
        let path = format!("/swap/matches/{id}");
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.get(&path, "tok", Some(&init)).1["status"], "proposed");
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        assert_eq!(e.get(&path, "tok", Some(&init)).1["status"], "accepted");

        let (s, msgs) = e.get(&format!("{path}/messages?after=0"), "tok", Some(&part));
        assert_eq!(s, 200, "{msgs}");
        let msgs = msgs["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["seq"], 1);
        assert_eq!(msgs[0]["from"], "initiator");
        assert_eq!(msgs[0]["body"]["type"], "accept");
        assert_eq!(msgs[1]["from"], "participant");
        let (_, later) = e.get(&format!("{path}/messages?after=1"), "tok", Some(&part));
        assert_eq!(later["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn decline_cancels_and_relays() {
        let e = edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &part, json!({"type": "decline"})).0, 200);
        let (_, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&init));
        assert_eq!(m["status"], "cancelled");
        let (_, msgs) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(&init));
        assert_eq!(msgs["messages"][0]["body"]["type"], "decline");
        // Too late to accept a cancelled match.
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 409);
    }

    /// [Final I3] Once both sides accepted, the initiator may fund at any
    /// moment: a participant that saw the XBT funding in the mempool must not
    /// be able to cancel the match (no strike) and lock the XBT for 144 h.
    #[test]
    fn decline_after_both_accepted_is_refused() {
        let e = edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "decline"})).0, 409);
        assert_eq!(e.msg(&id, &init, json!({"type": "decline"})).0, 409);
        let (_, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&init));
        assert_eq!(m["status"], "accepted");
        let (_, msgs) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(&init));
        assert!(msgs["messages"].as_array().unwrap().iter().all(|m| m["body"]["type"] != "decline"));
    }

    #[test]
    fn relay_message_shapes_are_checked() {
        let e = edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "keys", "xbt": PUBKEY, "btc": PUBKEY})).0, 200);
        assert_eq!(e.msg(&id, &init, json!({"type": "keys", "xbt": "00", "btc": PUBKEY})).0, 400);
        assert_eq!(e.msg(&id, &init, json!({"type": "hashlock", "hex": "11".repeat(32)})).0, 200);
        assert_eq!(e.msg(&id, &init, json!({"type": "hashlock", "hex": "11"})).0, 400);
        // Only the initiator knows the preimage.
        assert_eq!(e.msg(&id, &part, json!({"type": "hashlock", "hex": "11".repeat(32)})).0, 403);
        assert_eq!(e.msg(&id, &init, json!({"type": "gossip"})).0, 400);
    }

    #[test]
    fn long_poll_returns_new_message_without_waiting_full_timeout() {
        let e = edge();
        let (id, init, part) = e.matched();
        let url = e.url.clone();
        let (id2, init2) = (id.clone(), init.clone());
        let poster = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            call(
                "POST",
                &format!("{url}/swap/matches/{id2}/messages"),
                "tok",
                Some(&init2),
                Some(json!({"type": "keys", "xbt": PUBKEY, "btc": PUBKEY})),
            )
        });
        let t0 = Instant::now();
        let (s, msgs) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(&part));
        let took = t0.elapsed();
        assert_eq!(s, 200, "{msgs}");
        assert!(took < Duration::from_secs(1), "long-poll took {took:?}");
        assert!(took >= Duration::from_millis(150), "returned before the post: {took:?}");
        assert_eq!(msgs["messages"][0]["body"]["type"], "keys");
        assert_eq!(poster.join().unwrap().0, 200);
    }

    #[test]
    fn struck_out_token_gets_429() {
        let e = edge();
        let now = unix_ms() / 1000;
        for _ in 0..2 {
            e.sw.store.add_strike("tok-x", now).unwrap();
        }
        assert_eq!(e.offer("tok-x", "btc_to_xbt", 200_000, false).0, 200);
        e.sw.store.add_strike("tok-x", now).unwrap();
        assert_eq!(e.offer("tok-x", "btc_to_xbt", 200_000, false).0, 429);
        let (_, p) = e.offer("tok-a", "xbt_to_btc", 20_000_000, true);
        let code = p["share_code"].as_str().unwrap();
        assert_eq!(e.post(&format!("/swap/take/{code}"), "tok-x", None, json!({})).0, 429);
        assert_eq!(e.offer("tok-y", "btc_to_xbt", 200_000, false).0, 200);
    }

    fn offer_with_client(e: &Edge, token: &str, client_id: Option<&str>, private: bool) -> (u16, Value) {
        let mut body = json!({ "direction": "btc_to_xbt", "amount_sat": 200_000,
                               "max_slippage_bps": 100, "private": private });
        if let Some(c) = client_id {
            body["client_id"] = json!(c);
        }
        e.post("/swap/offers", token, None, body)
    }

    fn offers_of(e: &Edge, token: &str) -> usize {
        e.sw.store.all_offers().iter().filter(|o| o.token == token).count()
    }

    #[test]
    fn repeat_post_with_the_same_client_id_returns_the_same_offer() {
        let e = edge();
        let cid = "ab".repeat(16);
        let (s, a) = offer_with_client(&e, "tok-a", Some(&cid), true);
        assert_eq!(s, 200, "{a}");
        let (s, b) = offer_with_client(&e, "tok-a", Some(&cid), true);
        assert_eq!(s, 200, "{b}");
        assert_eq!(a["offer_id"], b["offer_id"]);
        assert_eq!(a["owner_secret"], b["owner_secret"]);
        assert_eq!(a["share_code"], b["share_code"]);
        assert!(a["share_code"].is_string());
        assert_eq!(offers_of(&e, "tok-a"), 1);
        // The replayed secret is the owner secret.
        let path = format!("/swap/offers/{}", a["offer_id"].as_str().unwrap());
        assert_eq!(e.get(&path, "tok-a", b["owner_secret"].as_str()).0, 200);
        // The store keeps only its hash.
        let stored = e.sw.store.offer(a["offer_id"].as_str().unwrap()).unwrap();
        let secret = hex32(a["owner_secret"].as_str().unwrap()).unwrap();
        assert_eq!(stored.secret_hash, <[u8; 32]>::from(Sha256::digest(secret)));
        assert_eq!(stored.client_id.as_deref(), Some(cid.as_str()));
    }

    #[test]
    fn a_different_client_id_or_token_makes_a_new_offer() {
        let e = edge();
        let (_, a) = offer_with_client(&e, "tok-a", Some("01"), false);
        let (_, b) = offer_with_client(&e, "tok-a", Some("02"), false);
        assert_ne!(a["offer_id"], b["offer_id"]);
        assert_ne!(a["owner_secret"], b["owner_secret"]);
        // Keyed by (token, client_id): another install's same id is its own offer.
        let (_, c) = offer_with_client(&e, "tok-b", Some("01"), false);
        assert_ne!(a["offer_id"], c["offer_id"]);
        assert_ne!(a["owner_secret"], c["owner_secret"]);
        assert_eq!(offers_of(&e, "tok-a"), 2);
        assert_eq!(offers_of(&e, "tok-b"), 1);
    }

    #[test]
    fn no_client_id_keeps_random_secrets_and_new_offers() {
        let e = edge();
        let (_, a) = offer_with_client(&e, "tok-a", None, false);
        let (_, b) = offer_with_client(&e, "tok-a", None, false);
        assert_ne!(a["offer_id"], b["offer_id"]);
        assert_ne!(a["owner_secret"], b["owner_secret"]);
        assert_eq!(offers_of(&e, "tok-a"), 2);
        let stored = e.sw.store.offer(a["offer_id"].as_str().unwrap()).unwrap();
        assert_eq!(stored.client_id, None);
    }

    #[test]
    fn a_replay_does_not_rerun_matching() {
        let e = edge();
        let (_, a) = offer_with_client(&e, "tok-a", Some("c1"), false);
        assert!(a["match_id"].is_null());
        // A counterparty that never went through matching.
        let (_, secret_hash) = new_secret().unwrap();
        let now = unix_ms() / 1000;
        e.sw.store
            .put_offer(&Offer {
                id: new_id().unwrap(),
                token: "tok-b".into(),
                secret_hash,
                direction: Direction::XbtToBtc,
                amount_sat: 20_000_000,
                max_slippage_bps: 100,
                created_at: now,
                expires_at: now + DEFAULT_EXPIRES_SECS,
                private: false,
                share_code: None,
                status: OfferStatus::Open,
                match_id: None,
                client_id: None,
            })
            .unwrap();
        let (s, again) = offer_with_client(&e, "tok-a", Some("c1"), false);
        assert_eq!(s, 200, "{again}");
        assert_eq!(again["offer_id"], a["offer_id"]);
        assert!(again["match_id"].is_null(), "{again}");
        assert!(e.sw.store.all_matches().is_empty());
    }

    #[test]
    fn a_replay_adopts_even_when_struck_out() {
        let e = edge();
        let (_, a) = offer_with_client(&e, "tok-x", Some("c1"), false);
        let now = unix_ms() / 1000;
        for _ in 0..MAX_STRIKES {
            e.sw.store.add_strike("tok-x", now).unwrap();
        }
        let (s, b) = offer_with_client(&e, "tok-x", Some("c1"), false);
        assert_eq!(s, 200, "{b}");
        assert_eq!(a["offer_id"], b["offer_id"]);
        // A new client id is a new offer, so the strike rule applies.
        assert_eq!(offer_with_client(&e, "tok-x", Some("c2"), false).0, 429);
    }

    #[test]
    fn client_id_must_be_short_hex() {
        let e = edge();
        assert_eq!(offer_with_client(&e, "tok-a", Some("not-hex"), false).0, 400);
        assert_eq!(offer_with_client(&e, "tok-a", Some(""), false).0, 400);
        assert_eq!(offer_with_client(&e, "tok-a", Some(&"a".repeat(65)), false).0, 400);
        assert_eq!(offer_with_client(&e, "tok-a", Some(&"a".repeat(64)), false).0, 200);
        assert_eq!(offers_of(&e, "tok-a"), 1);
    }

    fn txid(byte: u8) -> Txid {
        format!("{:02x}", byte).repeat(32).parse().unwrap()
    }

    fn spend(inputs: &[OutPoint]) -> String {
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs
                .iter()
                .map(|o| TxIn {
                    previous_output: *o,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output: vec![TxOut { value: Amount::from_sat(90_000), script_pubkey: ScriptBuf::new() }],
        };
        hex::encode(bitcoin::consensus::serialize(&tx))
    }

    /// Both sides accept, the initiator posts its XBT funding `txid(0xaa):1`.
    fn funded(e: &Edge) -> (String, String, String, OutPoint) {
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        // The participant sends BTC, not XBT; and can't fund before the initiator.
        let wrong = json!({"type": "funded", "chain": "xbt", "txid": htlc.txid.to_string(), "vout": 1});
        assert_eq!(e.msg(&id, &part, wrong).0, 400);
        let early = json!({"type": "funded", "chain": "btc", "txid": txid(0xbb).to_string(), "vout": 0});
        assert_eq!(e.msg(&id, &part, early).0, 409);
        let ok = json!({"type": "funded", "chain": "xbt", "txid": htlc.txid.to_string(), "vout": 1});
        assert_eq!(e.msg(&id, &init, ok).0, 200);
        (id, init, part, htlc)
    }

    const PREIMAGE: [u8; 32] = [7; 32];

    /// One side's swap keys: (XBT, BTC).
    struct Party {
        xbt: SwapKey,
        btc: SwapKey,
    }

    fn party(seed: u8) -> Party {
        let (btc, xbt) = network_params("bitcoin").unwrap();
        Party {
            xbt: SwapKey::from_secret(&xbt, [seed; 32]).unwrap(),
            btc: SwapKey::from_secret(&btc, [seed + 1; 32]).unwrap(),
        }
    }

    fn keys_msg(p: &Party) -> Value {
        json!({ "type": "keys", "xbt": hex::encode(p.xbt.pubkey().serialize()),
                "btc": hex::encode(p.btc.pubkey().serialize()) })
    }

    fn hashlock() -> sha256::Hash {
        sha256::Hash::hash(&PREIMAGE)
    }

    fn hashlock_msg() -> Value {
        json!({ "type": "hashlock", "hex": hex::encode(hashlock().to_byte_array()) })
    }

    fn terms_of(e: &Edge, id: &str, secret: &str) -> SwapTerms {
        let (s, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(secret));
        assert_eq!(s, 200, "{m}");
        serde_json::from_value(m["terms"].clone()).unwrap()
    }

    /// The initiator's XBT HTLC: the participant redeems, the initiator refunds.
    fn xbt_leg(t: &SwapTerms, init: &Party, part: &Party) -> HtlcContract {
        let (_, xbt) = network_params(&t.network).unwrap();
        let (redeem, refund) = (part.xbt.pubkey(), init.xbt.pubkey());
        HtlcContract::build(&xbt, hashlock(), &redeem, &refund, t.xbt_locktime, Amount::from_sat(t.xbt_sat))
    }

    /// The participant's BTC HTLC: the initiator redeems, the participant refunds.
    fn btc_leg(t: &SwapTerms, init: &Party, part: &Party) -> HtlcContract {
        let (btc, _) = network_params(&t.network).unwrap();
        let (redeem, refund) = (init.btc.pubkey(), part.btc.pubkey());
        HtlcContract::build(&btc, hashlock(), &redeem, &refund, t.btc_locktime, Amount::from_sat(t.btc_sat))
    }

    fn signed_refund(c: &HtlcContract, at: OutPoint, key: &SwapKey) -> Transaction {
        let mut tx = c.refund_tx(at, ScriptBuf::new(), Amount::from_sat(1_000)).unwrap();
        let sig = key.sign(&c.spend_sighash(&tx, 0).unwrap());
        c.finalize_refund(&mut tx, 0, &sig, &key.pubkey());
        tx
    }

    fn signed_redeem(c: &HtlcContract, at: OutPoint, key: &SwapKey) -> Transaction {
        let mut tx = c.redeem_tx(at, ScriptBuf::new(), Amount::from_sat(1_000)).unwrap();
        let sig = key.sign(&c.spend_sighash(&tx, 0).unwrap());
        c.finalize_redeem(&mut tx, 0, &sig, &key.pubkey(), &PREIMAGE);
        tx
    }

    fn tx_hex(tx: &Transaction) -> String {
        hex::encode(bitcoin::consensus::serialize(tx))
    }

    /// Both sides relay their keys and the initiator the hashlock.
    fn exchange_keys(e: &Edge, id: &str, init: &str, part: &str) -> (Party, Party) {
        let (i, p) = (party(1), party(3));
        assert_eq!(e.msg(id, init, keys_msg(&i)).0, 200);
        assert_eq!(e.msg(id, part, keys_msg(&p)).0, 200);
        assert_eq!(e.msg(id, init, hashlock_msg()).0, 200);
        (i, p)
    }

    #[test]
    fn exemption_needs_a_real_htlc_spend_of_the_recorded_outpoint() {
        let e = edge();
        let unrelated = OutPoint { txid: txid(0xcc), vout: 0 };
        let (id, init, part, htlc) = funded(&e);
        let (_, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&init));
        assert_eq!(m["status"], "initiator_funded");
        assert_eq!(m["xbt_funding"], json!({ "txid": htlc.txid.to_string(), "vout": 1 }));

        let t = terms_of(&e, &id, &init);
        let (i, p) = (party(1), party(3));
        let real = signed_refund(&xbt_leg(&t, &i, &p), htlc, &i.xbt);
        // Keys and hashlock not relayed yet: no script to check against.
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&real)));

        exchange_keys(&e, &id, &init, &part);
        assert!(is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&real)));
        // Recorded on XBT, so posting it to /btc/tx earns nothing.
        assert!(!is_swap_spend(&e.sw, Chain::Btc, &tx_hex(&real)));

        // The recorded outpoint spent as a plain P2WPKH coin: not the HTLC.
        let mut fake = real.clone();
        let mut w = Witness::new();
        w.push(real.input[0].witness.nth(0).unwrap());
        w.push(i.xbt.pubkey().serialize());
        fake.input[0].witness = w;
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&fake)));

        // Some other HTLC script (here: a later locktime) is not this match's.
        let mut other_terms = t.clone();
        other_terms.xbt_locktime += 1;
        let other = signed_refund(&xbt_leg(&other_terms, &i, &p), htlc, &i.xbt);
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&other)));

        // Mixed with an unrelated input, even one carrying the HTLC witness.
        let mut mixed = real.clone();
        let mut extra = real.input[0].clone();
        extra.previous_output = unrelated;
        mixed.input.push(extra);
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&mixed)));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &spend(&[htlc, unrelated])));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &spend(&[unrelated])));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &spend(&[htlc])));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &spend(&[])));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, "not hex"));
    }

    #[test]
    fn a_squatting_match_cannot_block_the_real_htlc_spend() {
        let e = edge();
        let (id, init, part, htlc) = funded(&e);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let real = signed_refund(&xbt_leg(&t, &i, &p), htlc, &i.xbt);

        // An attacker's match claims the victim's outpoint with its own keys,
        // and sorts before the victim's match.
        let mut squat = e.sw.store.get_match(&id).unwrap();
        squat.id = "!squat".into();
        let (ai, ap) = (party(5), party(7));
        let kx = |k: &Party| {
            (hex::encode(k.xbt.pubkey().serialize()), hex::encode(k.btc.pubkey().serialize()))
        };
        squat.keys = (Some(kx(&ai)), Some(kx(&ap)));
        e.sw.store.put_match(&squat).unwrap();
        assert_eq!(e.sw.store.matches_funded_by(Chain::Xbt, &htlc.txid.to_string(), 1).len(), 2);

        // The victim's real spend is still exempt ...
        assert!(is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&real)));
        // ... and the attacker's own script exempts nothing on a non-HTLC coin.
        let own = signed_refund(&xbt_leg(&t, &ai, &ap), htlc, &ai.xbt);
        let mut plain = own.clone();
        plain.input[0].previous_output = OutPoint { txid: txid(0xcc), vout: 0 };
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&plain)));
    }

    #[test]
    fn participant_funding_is_recorded() {
        let e = edge();
        let (id, init, part, _) = funded(&e);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let btc = OutPoint { txid: txid(0xbb), vout: 0 };
        let ok = json!({"type": "funded", "chain": "btc", "txid": btc.txid.to_string(), "vout": 0});
        assert_eq!(e.msg(&id, &part, ok).0, 200);
        let (_, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&init));
        assert_eq!(m["status"], "participant_funded");
        assert_eq!(m["btc_funding"], json!({ "txid": btc.txid.to_string(), "vout": 0 }));

        // The initiator's BTC redeem (preimage path) is exempt on /btc/tx only.
        let t = terms_of(&e, &id, &init);
        let redeem = signed_redeem(&btc_leg(&t, &i, &p), btc, &i.btc);
        assert!(is_swap_spend(&e.sw, Chain::Btc, &tx_hex(&redeem)));
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&redeem)));
    }

    #[test]
    fn keys_and_hashlock_are_set_once() {
        let e = edge();
        let (id, init, part) = e.matched();
        let (a, b) = (party(1), party(3));
        assert_eq!(e.msg(&id, &init, keys_msg(&a)).0, 200);
        // An identical repeat is fine; different keys are not.
        assert_eq!(e.msg(&id, &init, keys_msg(&a)).0, 200);
        assert_eq!(e.msg(&id, &init, keys_msg(&b)).0, 409);
        // The participant's keys are its own slot.
        assert_eq!(e.msg(&id, &part, keys_msg(&b)).0, 200);
        assert_eq!(e.msg(&id, &part, keys_msg(&a)).0, 409);

        assert_eq!(e.msg(&id, &init, hashlock_msg()).0, 200);
        assert_eq!(e.msg(&id, &init, hashlock_msg()).0, 200);
        assert_eq!(e.msg(&id, &init, json!({"type": "hashlock", "hex": "22".repeat(32)})).0, 409);

        // Only the first keys and hashlock were relayed.
        let (_, msgs) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(&part));
        let types: Vec<_> = msgs["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"]["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(types, ["keys", "keys", "hashlock"]);
    }

    fn poll_edge(cap: usize) -> Edge {
        let (sw, db) = new_swap(true);
        let mut sw = sw.with_long_poll_cap(cap);
        sw.long_poll = Duration::from_millis(600);
        edge_with((sw, db))
    }

    /// Starts a long-poll in the background; returns its thread (→ status, elapsed).
    fn bg_poll(e: &Edge, id: &str, secret: &str) -> thread::JoinHandle<(u16, Duration)> {
        let url = format!("{}/swap/matches/{id}/messages?after=0", e.url);
        let secret = secret.to_string();
        let h = thread::spawn(move || {
            let t0 = Instant::now();
            let (s, _) = call("GET", &url, "tok", Some(&secret), None);
            (s, t0.elapsed())
        });
        thread::sleep(Duration::from_millis(150));
        h
    }

    fn timed_poll(e: &Edge, id: &str, secret: &str) -> (u16, Value, Duration) {
        let t0 = Instant::now();
        let (s, v) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(secret));
        (s, v, t0.elapsed())
    }

    #[test]
    fn long_polls_beyond_the_global_cap_return_at_once() {
        let e = poll_edge(1);
        let (id, init, part) = e.matched();
        let first = bg_poll(&e, &id, &init);
        let (s, v, took) = timed_poll(&e, &id, &part);
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["messages"], json!([]));
        assert!(took < Duration::from_millis(300), "waited {took:?} over the cap");
        let (s, took) = first.join().unwrap();
        assert_eq!(s, 200);
        assert!(took >= Duration::from_millis(500), "first poll returned early: {took:?}");
        // The timed-out poll released its slot: the next one waits again.
        let (s, _, took) = timed_poll(&e, &id, &part);
        assert_eq!(s, 200);
        assert!(took >= Duration::from_millis(500), "slot not released: {took:?}");
    }

    #[test]
    fn one_long_poll_per_match_and_role() {
        let e = poll_edge(8);
        let (id, init, part) = e.matched();
        let first = bg_poll(&e, &id, &init);
        // A second poll by the same role on the same match doesn't wait…
        let (s, v, took) = timed_poll(&e, &id, &init);
        assert_eq!(s, 200, "{v}");
        assert!(took < Duration::from_millis(300), "duplicate poll waited {took:?}");
        // …but the other role's does.
        let other = bg_poll(&e, &id, &part);
        assert!(first.join().unwrap().1 >= Duration::from_millis(500));
        assert!(other.join().unwrap().1 >= Duration::from_millis(500));
        // Released once the first returned.
        let (_, _, took) = timed_poll(&e, &id, &init);
        assert!(took >= Duration::from_millis(500), "slot not released: {took:?}");
    }

    #[test]
    fn a_capped_poll_still_returns_pending_messages() {
        let e = poll_edge(1);
        let (id, init, part) = e.matched();
        let first = bg_poll(&e, &id, &init);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (s, v, _) = timed_poll(&e, &id, &part);
        assert_eq!(s, 200);
        assert_eq!(v["messages"][0]["body"]["type"], "accept");
        first.join().unwrap();
    }

    #[test]
    fn watch_job_needs_variants_spending_the_recorded_htlc() {
        let e = edge();
        let (id, init, part, htlc) = funded(&e);
        let job = |secret: &str, kind: &str, chain: &str, variants: Vec<String>| {
            e.post(
                "/swap/watch",
                "tok",
                Some(secret),
                json!({ "match_id": id, "chain": chain, "kind": kind,
                        "htlc_outpoint": { "txid": htlc.txid.to_string(), "vout": htlc.vout },
                        "not_before": 1_900_000_000u32, "variants": variants }),
            )
        };
        let other = OutPoint { txid: txid(0xcc), vout: 0 };
        assert_eq!(job(&init, "refund", "xbt", vec![spend(&[other])]).0, 400);
        assert_eq!(job(&init, "refund", "xbt", vec![spend(&[htlc, other])]).0, 400);
        assert_eq!(job(&init, "refund", "xbt", vec![]).0, 400);
        assert_eq!(job(&init, "refund", "xbt", vec!["zz".into()]).0, 400);
        // The participant can't refund the initiator's HTLC.
        assert_eq!(job(&part, "refund", "xbt", vec![spend(&[htlc])]).0, 403);
        assert_eq!(job(&"ab".repeat(32), "refund", "xbt", vec![spend(&[htlc])]).0, 403);
        // No BTC funding recorded yet.
        assert_eq!(job(&init, "redeem", "btc", vec![spend(&[htlc])]).0, 409);

        let (s, r) = job(&init, "refund", "xbt", vec![spend(&[htlc]), spend(&[htlc])]);
        assert_eq!(s, 200, "{r}");
        let jobs = e.sw.store.watch_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].htlc, (htlc.txid.to_string(), 1));
        assert_eq!(jobs[0].kind, store::WatchKind::Refund);
        assert_eq!(jobs[0].chain, wallet_core::Chain::Xbt);
        assert_eq!(jobs[0].variants.len(), 2);
        assert_eq!(jobs[0].not_before, 1_900_000_000);

        // The participant's preimage-less XBT redeem.
        let (s, r) = e.post(
            "/swap/watch",
            "tok",
            Some(&part),
            json!({ "match_id": id, "chain": "xbt", "kind": "redeem",
                    "htlc_outpoint": { "txid": htlc.txid.to_string(), "vout": htlc.vout },
                    "not_before": 0, "variants": [spend(&[htlc]), spend(&[htlc])],
                    "sigs": ["30", "31"], "pubkey": PUBKEY, "witness_script": "51" }),
        );
        assert_eq!(s, 200, "{r}");
        assert!(e.sw.store.watch_jobs().iter().any(|j| matches!(
            &j.kind,
            store::WatchKind::WaitingRedeem { pubkey_hex, sigs_hex, .. }
                if pubkey_hex == PUBKEY && sigs_hex == &["30", "31"]
        )));
        // One signature per variant, in order: anything else is refused.
        let waiting = |sigs: Value, variants: Vec<String>| {
            e.post(
                "/swap/watch",
                "tok",
                Some(&part),
                json!({ "match_id": id, "chain": "xbt", "kind": "redeem",
                        "htlc_outpoint": { "txid": htlc.txid.to_string(), "vout": htlc.vout },
                        "not_before": 0, "variants": variants,
                        "sigs": sigs, "pubkey": PUBKEY, "witness_script": "51" }),
            )
            .0
        };
        assert_eq!(waiting(json!(["30"]), vec![spend(&[htlc]), spend(&[htlc])]), 400);
        assert_eq!(waiting(json!(["30", "31"]), vec![spend(&[htlc])]), 400);
        assert_eq!(waiting(json!([]), vec![spend(&[htlc])]), 400);
        assert_eq!(waiting(json!(["zz"]), vec![spend(&[htlc])]), 400);
    }

    /// A chain with a fixed set of unspent outputs.
    #[derive(Default)]
    struct StubChain {
        txouts: Mutex<std::collections::HashMap<(Chain, String, u32), watchtower::TxOutInfo>>,
    }

    impl StubChain {
        fn put(&self, chain: Chain, at: OutPoint, value_sat: u64, spk: &ScriptBuf) {
            self.txouts.lock().unwrap().insert(
                (chain, at.txid.to_string(), at.vout),
                watchtower::TxOutInfo {
                    value_sat,
                    scriptpubkey_hex: hex::encode(spk.as_bytes()),
                    confirmations: 0,
                },
            );
        }
    }

    impl watchtower::ChainReader for StubChain {
        fn txout(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<watchtower::TxOutInfo>> {
            Ok(self.txouts.lock().unwrap().get(&(chain, txid.to_string(), vout)).cloned())
        }
        fn spending_tx_hex(&self, _: Chain, _: &str, _: u32) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn spend_confirmed(&self, _: Chain, _: &str, _: u32) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn feerate(&self, _: Chain) -> anyhow::Result<u64> {
            Ok(1)
        }
        fn broadcast(&self, _: Chain, _: &str) -> anyhow::Result<String> {
            anyhow::bail!("not in these tests")
        }
        fn now(&self) -> u64 {
            unix_now()
        }
    }

    fn chain_edge() -> (Edge, Arc<StubChain>) {
        let chain = Arc::new(StubChain::default());
        let (sw, db) = new_swap(true);
        (edge_with((sw.with_chain_reader(chain.clone()), db)), chain)
    }

    fn refund_watch(e: &Edge, id: &str, secret: &str, at: OutPoint, refund: &Transaction) -> (u16, Value) {
        e.post(
            "/swap/watch",
            "tok",
            Some(secret),
            json!({ "match_id": id, "chain": "xbt", "kind": "refund",
                    "htlc_outpoint": { "txid": at.txid.to_string(), "vout": at.vout },
                    "not_before": 1_900_000_000u32, "variants": [tx_hex(refund)] }),
        )
    }

    /// Our XBT funding broadcast just before `initiator_fund_by`, and the
    /// deadline tick cancelled the match before our `funded` landed (a
    /// decline is no longer possible once both accepted): the refund watch
    /// alone must get our refund to the watchtower, and our own refund
    /// broadcast must stay fee-exempt.
    #[test]
    fn refund_watch_of_a_real_htlc_is_accepted_on_a_cancelled_match_without_funded() {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let leg = xbt_leg(&t, &i, &p);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        chain.put(Chain::Xbt, htlc, t.xbt_sat, &leg.script_pubkey);
        matcher::tick(&e.sw.store, t.initiator_fund_by as u64 + 1);
        let refund = signed_refund(&leg, htlc, &i.xbt);
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&refund)));

        let (s, r) = refund_watch(&e, &id, &init, htlc, &refund);
        assert_eq!(s, 200, "{r}");
        let (_, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&init));
        assert_eq!(m["status"], "cancelled", "the status is not changed");
        assert_eq!(m["xbt_funding"], json!({ "txid": htlc.txid.to_string(), "vout": 1 }));
        assert_eq!(e.sw.store.watch_jobs().len(), 1);
        assert!(is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&refund)));
        // An identical repeat is still fine.
        assert_eq!(refund_watch(&e, &id, &init, htlc, &refund).0, 200);
    }

    #[test]
    fn refund_watch_of_an_unrecorded_outpoint_needs_the_exact_htlc_output() {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let leg = xbt_leg(&t, &i, &p);
        let mut other_terms = t.clone();
        other_terms.xbt_locktime += 1;
        let (wrong_spk, short, long, unknown) =
            (OutPoint { txid: txid(0xa1), vout: 0 }, OutPoint { txid: txid(0xa2), vout: 0 },
             OutPoint { txid: txid(0xa3), vout: 0 }, OutPoint { txid: txid(0xa4), vout: 0 });
        chain.put(Chain::Xbt, wrong_spk, t.xbt_sat, &xbt_leg(&other_terms, &i, &p).script_pubkey);
        chain.put(Chain::Xbt, short, t.xbt_sat - 1, &leg.script_pubkey);
        chain.put(Chain::Xbt, long, t.xbt_sat + 1, &leg.script_pubkey);
        for at in [wrong_spk, short, long, unknown] {
            let (s, r) = refund_watch(&e, &id, &init, at, &signed_refund(&leg, at, &i.xbt));
            assert_eq!(s, 409, "{at}: {r}");
        }
        assert!(e.sw.store.get_match(&id).unwrap().xbt_funding.is_none());
        assert!(e.sw.store.watch_jobs().is_empty());
    }

    #[test]
    fn refund_watch_of_an_unrecorded_outpoint_needs_both_keys_and_the_hashlock() {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        let (i, p) = (party(1), party(3));
        let t = terms_of(&e, &id, &init);
        let leg = xbt_leg(&t, &i, &p);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        chain.put(Chain::Xbt, htlc, t.xbt_sat, &leg.script_pubkey);
        let refund = signed_refund(&leg, htlc, &i.xbt);
        assert_eq!(e.msg(&id, &init, keys_msg(&i)).0, 200);
        assert_eq!(e.msg(&id, &init, hashlock_msg()).0, 200);
        // The participant's keys are missing.
        assert_eq!(refund_watch(&e, &id, &init, htlc, &refund).0, 409);
        assert!(e.sw.store.get_match(&id).unwrap().xbt_funding.is_none());
        assert_eq!(e.msg(&id, &part, keys_msg(&p)).0, 200);
        assert_eq!(refund_watch(&e, &id, &init, htlc, &refund).0, 200);
    }

    /// A redeem watch names the COUNTERPARTY's leg: it must never record a
    /// funding (an unconfirmed look-alike could be double-spent later).
    #[test]
    fn redeem_watches_never_record_an_unrecorded_counterparty_leg() {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let xleg = xbt_leg(&t, &i, &p);
        let bleg = btc_leg(&t, &i, &p);
        let (xh, bh) = (OutPoint { txid: txid(0xaa), vout: 1 }, OutPoint { txid: txid(0xbb), vout: 0 });
        chain.put(Chain::Xbt, xh, t.xbt_sat, &xleg.script_pubkey);
        chain.put(Chain::Btc, bh, t.btc_sat, &bleg.script_pubkey);
        let post = |secret: &str, leg: &str, at: OutPoint, extra: Value| {
            let mut b = json!({ "match_id": id, "chain": leg, "kind": "redeem",
                    "htlc_outpoint": { "txid": at.txid.to_string(), "vout": at.vout },
                    "not_before": 0, "variants": [spend(&[at])] });
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            e.post("/swap/watch", "tok", Some(secret), b).0
        };
        let waiting = json!({ "sigs": ["30"], "pubkey": PUBKEY, "witness_script": "51" });
        for (secret, leg, at) in [(&part, "xbt", xh), (&init, "btc", bh)] {
            assert_eq!(post(secret, leg, at, json!({})), 409, "redeem {leg}");
            assert_eq!(post(secret, leg, at, waiting.clone()), 409, "waiting redeem {leg}");
        }
        let m = e.sw.store.get_match(&id).unwrap();
        assert!(m.xbt_funding.is_none() && m.btc_funding.is_none());
        assert!(e.sw.store.watch_jobs().is_empty());
        // The non-owner can't post a refund of the leg either.
        assert_eq!(refund_watch(&e, &id, &part, xh, &signed_refund(&xleg, xh, &i.xbt)).0, 403);
        assert!(e.sw.store.get_match(&id).unwrap().xbt_funding.is_none());
    }

    /// The phone posts its refund watch before `funded`: `funded` with the
    /// same outpoint must still move the match on.
    #[test]
    fn funded_after_the_refund_watch_recorded_it_moves_the_match_on() {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let leg = xbt_leg(&t, &i, &p);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        chain.put(Chain::Xbt, htlc, t.xbt_sat, &leg.script_pubkey);
        assert_eq!(refund_watch(&e, &id, &init, htlc, &signed_refund(&leg, htlc, &i.xbt)).0, 200);
        assert_eq!(e.sw.store.get_match(&id).unwrap().status, MatchStatus::Accepted);

        let ok = json!({"type": "funded", "chain": "xbt", "txid": htlc.txid.to_string(), "vout": 1});
        let (s, r) = e.msg(&id, &init, ok.clone());
        assert_eq!(s, 200, "{r}");
        assert!(r["seq"].is_u64(), "relayed: {r}");
        assert_eq!(e.sw.store.get_match(&id).unwrap().status, MatchStatus::InitiatorFunded);
        // A repeat is a no-op.
        assert_eq!(e.msg(&id, &init, ok).1["seq"], Value::Null);
    }

    fn bare_state(swap: Option<Swap>) -> (crate::State, crate::test_tmp::TempDir) {
        let dir = temp_dir("state");
        let st = crate::State {
            secret: b"0123456789abcdef0123456789abcdef".to_vec(),
            require_token: false,
            trust_forwarded_for: false,
            allow_origin: "*".into(),
            xbt: None,
            btc: None,
            btc_fallbacks: Vec::new(),
            btc_tip: None,
            btc_broadcast: None,
            btc_price: None,
            xbt_price: None,
            btc_haskoin: None,
            btc_upstream_scan: false,
            btc_pacer: None,
            limiter: crate::RateLimiter::new(600, 300),
            register_limiter: crate::RateLimiter::new(10, 10),
            crash_limiter: crate::RateLimiter::new(2, 8),
            crash_log: None,
            pricing: None,
            cache: crate::Cache::new(16, &dir.join("cache")),
            btc_history: None,
            btc_scan_cache: None,
            btc_mempool_ws: None,
            metrics: crate::Metrics::default(),
            swap,
        };
        (st, dir)
    }

    fn serve_handle(st: crate::State) -> String {
        let st = Arc::new(st);
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let reply = crate::handle(&mut req, &st);
                let _ = crate::respond(req, reply, "*");
            }
        });
        url
    }

    #[test]
    fn swap_routes_404_when_disabled() {
        let (st, _state_dir) = bare_state(None);
        let url = serve_handle(st);
        let token = crate::token::issue(b"0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(call("GET", &format!("{url}/swap/quote"), &token, None, None).0, 404);
        assert_eq!(call("POST", &format!("{url}/swap/offers"), &token, None, Some(json!({}))).0, 404);

        // Enabled: the install token is required even when the edge doesn't require it elsewhere.
        let (sw, _db) = new_swap(true);
        let (st, _state_dir2) = bare_state(Some(sw));
        let url = serve_handle(st);
        assert_eq!(call("GET", &format!("{url}/swap/quote"), "bogus", None, None).0, 401);
        assert_eq!(call("GET", &format!("{url}/swap/quote"), &token, None, None).0, 200);
        assert_eq!(call("GET", &format!("{url}/swap/nope"), &token, None, None).0, 404);
    }
}
