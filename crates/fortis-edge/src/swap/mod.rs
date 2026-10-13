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

/// A swap event for the journal: `fortis-edge: swap <id>: <what>`. Ids,
/// amounts, outpoints, statuses and reasons only — never secrets, tokens or keys.
macro_rules! swap_event {
    ($id:expr, $($arg:tt)*) => {
        eprintln!("fortis-edge: swap {}: {}", $id, format_args!($($arg)*))
    };
}

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
use wallet_core::swap_terms::{btc_for_xbt, network_params, MAX_PRICE_SAT_PER_XBT, MIN_BTC_LEG_SAT};
use wallet_core::{Chain, HtlcContract};

use crate::{err, Reply};
use matcher::{btc_leg_value, xbt_for_btc};
use neoxex::NeoxEx;
use watchtower::ChainReader;
use store::{
    new_id, tag_hash, Direction, Match, MatchStatus, Offer, OfferStatus, Store, WatchJob, WatchKind,
};

pub const DEFAULT_EXPIRES_SECS: u64 = 24 * 3600;
const MAX_EXPIRES_SECS: u64 = 7 * 24 * 3600;
/// Missed funding deadlines in 24 h that bar a token from new offers.
pub const MAX_STRIKES: u32 = 3;

/// Offers one install token may have waiting for a match at once. Real users
/// have one or two; this stops one token flooding the book (every watchtower
/// pass pairs each open offer against the rest).
pub const MAX_OPEN_OFFERS: usize = 10;
const LONG_POLL: Duration = Duration::from_secs(25);
/// Half the default 4 HTTP workers.
const DEFAULT_LONG_POLL_CAP: usize = 2;
/// Watch jobs carry a handful of signed transactions.
const MAX_BODY: u64 = 256 * 1024;
/// A relay message's body: the largest real one (keys) is ~170 bytes.
pub const MAX_MESSAGE_BODY: u64 = 4 * 1024;
/// Messages one match's relay holds. A swap needs about ten (two accepts,
/// two keys, the hashlock, two fundeds, the edge's deadline): a backstop.
pub const MAX_MESSAGES_PER_MATCH: usize = 200;
const MAX_VARIANTS: usize = 16;
/// Crockford base32: no I, L, O, U.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Longest `client_id` (hex digits) an offer may carry.
const MAX_CLIENT_ID: usize = 64;

pub struct Swap {
    store: Arc<Store>,
    neoxex: NeoxEx,
    fee_bps: u32,
    /// Largest BTC leg of one swap; `None` = no maximum.
    max_btc_sat: Option<u64>,
    /// Terms network: "bitcoin" | "regtest".
    network: String,
    /// The service-fee address; both legs pay their commission to it.
    fee_address: String,
    /// How long `GET …/messages` waits for a message.
    long_poll: Duration,
    polls: PollGate,
    /// Per-offer budget for phones' `POST /swap/offers/{id}/log` lines.
    log_quota: LogQuota,
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
        max_btc_sat: Option<u64>,
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
            log_quota: LogQuota::default(),
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

    /// The BTC-leg cap the matcher applies: the configured one, or none.
    fn cap(&self) -> u64 {
        self.max_btc_sat.unwrap_or(u64::MAX)
    }

    /// What the watchtower's rematch pass builds terms with.
    pub fn matching(&self) -> watchtower::Matching {
        watchtower::Matching {
            max_btc_sat: self.cap(),
            network: self.network.clone(),
            fee_address: self.fee_address.clone(),
        }
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

/// A JSON body of at most `limit` bytes: 413 over it, 400 if it isn't JSON.
fn read_small_json(req: &mut Request, limit: u64) -> Result<Value, Reply> {
    let mut body = Vec::new();
    if req.as_reader().take(limit + 1).read_to_end(&mut body).is_err() {
        return Err(err(400, "couldn't read the request body"));
    }
    if body.len() as u64 > limit {
        return Err(err(413, &format!("the body is over {limit} bytes")));
    }
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
        (Method::Get, ["book"]) => book(token, sw),
        (Method::Post, ["offers"]) => create_offer(req, token, sw),
        (Method::Get, ["offers", id]) => get_offer(req, id, sw),
        (Method::Post, ["offers", id, "log"]) => phone_log(req, id, sw),
        (Method::Post, ["offers", id, "take"]) => take_offer(req, id, token, sw),
        (Method::Delete, ["offers", id]) => cancel_offer(req, id, sw),
        (Method::Post, ["take", code]) => take(req, code, token, sw),
        (Method::Post, ["recover"]) => recover(req, sw),
        (Method::Get, ["matches", id]) => get_match(req, id, sw),
        (Method::Post, ["matches", id, "messages"]) => post_message(req, id, sw),
        (Method::Get, ["matches", id, "messages"]) => poll_messages(req, id, sw),
        (Method::Post, ["watch"]) => watch(req, sw),
        _ => err(404, "no such route"),
    }
}

/// The NeoxEX reference (if any) next to the public book's best prices.
/// Never fails on NeoxEX: with it down the reference is `null`.
fn quote(sw: &Swap) -> Reply {
    let reference = sw.neoxex.reference();
    let now = unix_now();
    let book = |d: Direction| -> Vec<u64> {
        sw.store
            .open_offers(d)
            .into_iter()
            .filter(|o| !o.private && o.expires_at > now)
            .map(|o| o.price_sat_per_xbt)
            .collect()
    };
    let (asks, bids) = (book(Direction::XbtToBtc), book(Direction::BtcToXbt));
    Reply::Json(
        200,
        json!({
            "reference_price": reference.map(|r| r.price_sat_per_xbt),
            "reference_ok": reference.is_some_and(|r| r.ok),
            "best": {
                "xbt_to_btc": asks.iter().min(),
                "btc_to_xbt": bids.iter().max(),
            },
            "fee_bps": sw.fee_bps,
            "min_btc_sat": MIN_BTC_LEG_SAT,
            "max_btc_sat": sw.max_btc_sat,
            "open": {
                "xbt_to_btc": asks.len(),
                "btc_to_xbt": bids.len(),
            },
        }),
    )
}

/// Most offers `GET /swap/book` lists per direction.
const BOOK_MAX_PER_SIDE: usize = 50;

/// `GET /swap/book`: the open public offers a caller could take — best price
/// first (lowest ask, highest bid), then oldest; at most [`BOOK_MAX_PER_SIDE`]
/// per direction. Left out: private, expired, below-minimum, struck-out and the
/// caller's own offers. An entry carries only what a taker needs: no token,
/// secret hash, share code or client id (an offer id grants nothing alone).
fn book(token: &str, sw: &Swap) -> Reply {
    let now = unix_now();
    let side = |d: Direction| -> Vec<Value> {
        let mut offers: Vec<Offer> = sw
            .store
            .open_offers(d)
            .into_iter()
            .filter(|o| !o.private && o.expires_at > now && o.token != token)
            .filter(|o| btc_leg_value(o.direction, o.amount_sat, o.price_sat_per_xbt) >= MIN_BTC_LEG_SAT)
            .filter(|o| !struck_out(sw, &o.token))
            .collect();
        // open_offers is oldest first; a stable sort by price keeps that within a price.
        match d {
            Direction::XbtToBtc => offers.sort_by_key(|o| o.price_sat_per_xbt),
            Direction::BtcToXbt => offers.sort_by_key(|o| std::cmp::Reverse(o.price_sat_per_xbt)),
        }
        offers
            .into_iter()
            .take(BOOK_MAX_PER_SIDE)
            .map(|o| {
                let (btc_sat, xbt_sat) = match o.direction {
                    Direction::XbtToBtc => (btc_leg_value(o.direction, o.amount_sat, o.price_sat_per_xbt), o.amount_sat),
                    Direction::BtcToXbt => (o.amount_sat, xbt_for_btc(o.amount_sat, o.price_sat_per_xbt)),
                };
                json!({
                    "id": o.id,
                    "direction": o.direction,
                    "amount_sat": o.amount_sat,
                    "price_sat_per_xbt": o.price_sat_per_xbt,
                    "btc_sat": btc_sat,
                    "xbt_sat": xbt_sat,
                    "expires_at": o.expires_at,
                })
            })
            .collect()
    };
    Reply::Json(
        200,
        json!({ "xbt_to_btc": side(Direction::XbtToBtc), "btc_to_xbt": side(Direction::BtcToXbt) }),
    )
}

/// Offers `token` has waiting for a match (open and not expired).
fn open_offers_of(sw: &Swap, token: &str) -> usize {
    let now = unix_now();
    sw.store
        .all_offers()
        .iter()
        .filter(|o| o.token == token && o.status == OfferStatus::Open && o.expires_at > now)
        .count()
}

/// A short, non-reversible tag for a token in the journal (never the token).
fn token_tag(token: &str) -> String {
    format!("token:{}", &hex::encode(Sha256::digest(token.as_bytes()))[..12])
}

fn struck_out(sw: &Swap, token: &str) -> bool {
    sw.store.strikes(token, unix_now()) >= MAX_STRIKES
}

const STRUCK_OUT: &str = "too many missed funding deadlines — try again in 24 hours";

#[derive(Deserialize)]
struct NewOffer {
    direction: Direction,
    amount_sat: u64,
    /// Missing reads as 0, which is refused.
    #[serde(default)]
    price_sat_per_xbt: u64,
    expires_in: Option<u64>,
    #[serde(default)]
    private: bool,
    /// Idempotency key: hex, at most 64 digits.
    #[serde(default)]
    client_id: Option<String>,
    /// The swap's recovery tag (64 hex digits), see [`Offer::recovery_tag`].
    #[serde(default)]
    recovery_tag: Option<String>,
}

/// Refused: a recovery tag (so a swap index, so its keys and preimage) is used once.
const TAG_USED: &str = store::TAG_USED;
/// Tags one `POST /swap/recover` may ask about.
pub const MAX_RECOVER_TAGS: usize = 100;

/// A request's recovery tag: none, or exactly 64 hex digits (lowercased).
/// The offer keeps only its [`tag_hash`] (see [`claimed_tag`]).
fn parse_tag(t: Option<&str>) -> Result<Option<String>, Reply> {
    match t {
        None => Ok(None),
        Some(t) if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(Some(t.to_ascii_lowercase())),
        Some(_) => Err(err(400, "recovery_tag must be 64 hex digits")),
    }
}

/// A request's recovery tag as the offer stores it (its hash): `400` if
/// malformed, `409` if any offer ever held it (purged ones too). The store
/// re-checks when the offer is saved, so a race can't share a tag either.
fn claimed_tag(t: Option<&str>, sw: &Swap) -> Result<Option<String>, Reply> {
    let Some(tag) = parse_tag(t)? else { return Ok(None) };
    let hash = tag_hash(&tag);
    if sw.store.tag_used(&hash) {
        return Err(err(409, TAG_USED));
    }
    Ok(Some(hash))
}

/// A store error, as the reply: a tag claimed meanwhile is the `409`.
fn store_failed(e: anyhow::Error) -> Reply {
    if e.to_string() == TAG_USED {
        err(409, TAG_USED)
    } else {
        internal(e)
    }
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
        // A replay adopts the offer as it now stands: no strike, price or
        // amount check, and no matching.
        if let Some(o) = sw.store.offer_by_client(token, c) {
            return offer_created(sw, &o, &client_secret(sw, token, c).0);
        }
    }
    let recovery_tag = match claimed_tag(n.recovery_tag.as_deref(), sw) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if struck_out(sw, token) {
        return err(429, STRUCK_OUT);
    }
    if open_offers_of(sw, token) >= MAX_OPEN_OFFERS {
        swap_event!(token_tag(token), "offer refused: {MAX_OPEN_OFFERS} open offers already");
        return err(429, &format!("you have {MAX_OPEN_OFFERS} open offers waiting: cancel one or wait for a match"));
    }
    if n.price_sat_per_xbt == 0 || n.price_sat_per_xbt > MAX_PRICE_SAT_PER_XBT {
        return err(400, &format!("price_sat_per_xbt must be 1..={MAX_PRICE_SAT_PER_XBT}"));
    }
    let expires_in = n.expires_in.unwrap_or(DEFAULT_EXPIRES_SECS);
    if expires_in == 0 || expires_in > MAX_EXPIRES_SECS {
        return err(400, "expires_in out of range");
    }
    let btc_leg = btc_leg_value(n.direction, n.amount_sat, n.price_sat_per_xbt);
    if btc_leg < MIN_BTC_LEG_SAT {
        return err(400, &format!("amount must be worth at least {MIN_BTC_LEG_SAT} BTC sat"));
    }
    if let Some(max) = sw.max_btc_sat.filter(|&m| btc_leg > m) {
        return err(400, &format!("amount must be worth at most {max} BTC sat"));
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
            price_sat_per_xbt: n.price_sat_per_xbt,
            created_at: now,
            expires_at: now + expires_in,
            private: n.private,
            share_code,
            status: OfferStatus::Open,
            match_id: None,
            client_id: client_id.clone(),
            recovery_tag: recovery_tag.clone(),
        };
        sw.store.put_offer(&offer)?;
        swap_event!(
            offer.id, "offer posted: {:?} {} sat at {} sat/XBT{}",
            offer.direction, offer.amount_sat, offer.price_sat_per_xbt, if offer.private { " (private)" } else { "" }
        );
        Ok((offer, secret, true))
    })();
    let (offer, secret, fresh) = match made {
        Ok(x) => x,
        Err(e) => return store_failed(e),
    };

    if fresh && !offer.private {
        // One match per call; keep going while a remainder is still open.
        loop {
            match matcher::match_offer(
                &sw.store,
                &offer,
                now,
                sw.cap(),
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
        "price_sat_per_xbt": o.price_sat_per_xbt,
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

/// Most lines one `POST /swap/offers/{id}/log` may carry.
const LOG_MAX_LINES: usize = 50;
/// Longer lines are cut to this many characters.
const LOG_MAX_CHARS: usize = 300;
/// Lines one offer may log per minute: a step logs a handful; this only stops a flood.
const LOG_LINES_PER_MIN: u32 = 300;

/// Lines logged per offer in the current minute.
#[derive(Default)]
struct LogQuota(Mutex<std::collections::HashMap<String, (u64, u32)>>);

impl LogQuota {
    /// Take `n` lines of `offer`'s budget for the minute of `now`; `false` if over.
    fn take(&self, offer: &str, n: usize, now: u64) -> bool {
        let minute = now / 60;
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > 10_000 {
            map.retain(|_, (m, _)| *m == minute);
        }
        let slot = map.entry(offer.to_string()).or_insert((minute, 0));
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        let n = n.min(u32::MAX as usize) as u32;
        if slot.1.saturating_add(n) > LOG_LINES_PER_MIN {
            return false;
        }
        slot.1 += n;
        true
    }
}

/// `{"lines": ["…", …]}` → at most [`LOG_MAX_LINES`] lines, each cut to
/// [`LOG_MAX_CHARS`], with control characters made spaces and anything else
/// but printable ASCII written as `\u{…}` (a phone can't forge, split or
/// visually reorder journal lines — no right-to-left overrides, no
/// zero-width characters).
fn log_lines(body: &Value) -> Result<Vec<String>, Reply> {
    let bad = || err(400, "lines must be an array of strings");
    let arr = body["lines"].as_array().ok_or_else(bad)?;
    if arr.len() > LOG_MAX_LINES {
        return Err(err(400, &format!("at most {LOG_MAX_LINES} lines")));
    }
    arr.iter()
        .map(|l| {
            let l = l.as_str().ok_or_else(bad)?;
            Ok(l.chars()
                .flat_map(|c| match c {
                    c if c.is_control() => vec![' '],
                    ' '..='~' => vec![c],
                    c => format!("\\u{{{:x}}}", c as u32).chars().collect(),
                })
                .take(LOG_MAX_CHARS)
                .collect())
        })
        .collect()
}

/// `POST /swap/offers/{id}/log`: a phone's swap-step trace for this offer,
/// written to the journal next to the edge's own events, so one
/// `journalctl -u fortis-edge` shows both sides of a swap. The offer's owner
/// secret is required; lines are capped per request and per minute.
fn phone_log(req: &mut Request, id: &str, sw: &Swap) -> Reply {
    let Some(o) = sw.store.offer(id) else {
        return err(404, "no such offer");
    };
    if !owns_offer(req, &o) {
        return err(403, "wrong or missing X-Swap-Secret");
    }
    let body: Value = match read_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let lines = match log_lines(&body) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if !sw.log_quota.take(id, lines.len(), unix_now()) {
        return err(429, "too many log lines this minute");
    }
    let tag = match &o.match_id {
        Some(m) => format!("{id} (match {m})"),
        None => id.to_string(),
    };
    for l in lines {
        eprintln!("fortis-edge: swap phone {tag}: {l}");
    }
    Reply::Json(200, json!({ "ok": true }))
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
    swap_event!(o.id, "offer cancelled by its owner ({} sat left unmatched)", o.amount_sat);
    match sw.store.put_offer(&o) {
        Ok(()) => Reply::Json(200, offer_json(sw, &o)),
        Err(e) => internal(e),
    }
}

/// Takes the whole private offer behind `code`, at the maker's price, with a
/// one-shot offer of the opposite direction. The request body is ignored but
/// for an optional `recovery_tag`.
fn take(req: &mut Request, code: &str, token: &str, sw: &Swap) -> Reply {
    #[derive(Deserialize)]
    struct TakeCode {
        #[serde(default)]
        recovery_tag: Option<String>,
    }
    let body: TakeCode = match read_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let recovery_tag = match claimed_tag(body.recovery_tag.as_deref(), sw) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if struck_out(sw, token) {
        return err(429, STRUCK_OUT);
    }
    let code = normalize_code(code);
    let Some(maker) = sw.store.offer_by_code(&code) else {
        return err(404, "unknown share code");
    };
    let price = maker.price_sat_per_xbt;
    let direction = maker.direction.opposite();
    // The maker's amount on the taker's own send chain.
    let amount_sat = match direction {
        Direction::XbtToBtc => xbt_for_btc(maker.amount_sat, price),
        Direction::BtcToXbt => btc_for_xbt(maker.amount_sat, price),
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
        price_sat_per_xbt: price,
        created_at: now,
        expires_at: now + DEFAULT_EXPIRES_SECS,
        // Never on the public book: it exists only for this take.
        private: true,
        share_code: None,
        status: OfferStatus::Open,
        match_id: None,
        client_id: None,
        recovery_tag,
    };
    let m = match matcher::take_private(
        &sw.store,
        &code,
        &taker,
        now,
        sw.cap(),
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
    swap_event!(m.id, "private match taken by code: offer {} with offer {}", m.initiator_offer, m.participant_offer);
    Reply::Json(200, json!({ "match_id": m.id, "owner_secret": secret, "offer_id": taker.id }))
}

#[derive(Deserialize)]
struct TakeReq {
    /// On the taker's own send chain.
    amount_sat: u64,
    /// The price the taker saw; a different one is refused.
    price_sat_per_xbt: u64,
    client_id: String,
    #[serde(default)]
    recovery_tag: Option<String>,
}

/// `POST /swap/offers/{id}/take`: take all or part of a listed public offer at
/// its own price. The taker's side is an unlisted offer paired with exactly
/// this one; a repeat with the same `client_id` returns the same match.
fn take_offer(req: &mut Request, id: &str, token: &str, sw: &Swap) -> Reply {
    let t: TakeReq = match read_json(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let client_id = t.client_id.to_ascii_lowercase();
    if client_id.is_empty() || client_id.len() > MAX_CLIENT_ID || !client_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return err(400, "client_id must be 1..=64 hex digits");
    }
    let (secret, secret_hash) = client_secret(sw, token, &client_id);
    // A repeat (a lost response, a retry): the same take, never a second one.
    if let Some(o) = sw.store.offer_by_client(token, &client_id) {
        let match_id = o.match_id.clone().or_else(|| offer_matches(sw, &o.id).last().map(|m| m.id.clone()));
        return match match_id {
            Some(m) => Reply::Json(200, json!({ "match_id": m, "owner_secret": secret, "offer_id": o.id })),
            None => err(409, "offer is no longer available"),
        };
    }
    let recovery_tag = match claimed_tag(t.recovery_tag.as_deref(), sw) {
        Ok(tag) => tag,
        Err(r) => return r,
    };
    if struck_out(sw, token) {
        return err(429, STRUCK_OUT);
    }
    let Some(maker) = sw.store.offer(id) else {
        return err(404, "no such offer");
    };
    let now = unix_now();
    // What the offer itself rules out comes first (re-checked under the
    // matching lock by `take_public`); then the amount.
    if maker.private {
        return err(409, "that offer is private: take it with its share code");
    }
    if maker.status != OfferStatus::Open || maker.expires_at <= now {
        return err(409, "offer is no longer available");
    }
    if maker.token == token {
        return err(409, "cannot take your own offer");
    }
    if maker.price_sat_per_xbt != t.price_sat_per_xbt {
        return err(409, "price changed");
    }
    let direction = maker.direction.opposite();
    let price = t.price_sat_per_xbt;
    // The taker's amount, its BTC leg, and what the maker still holds on the taker's chain.
    let btc_leg = btc_leg_value(direction, t.amount_sat, price);
    let available = match direction {
        Direction::XbtToBtc => xbt_for_btc(maker.amount_sat, maker.price_sat_per_xbt),
        Direction::BtcToXbt => btc_for_xbt(maker.amount_sat, maker.price_sat_per_xbt),
    };
    if btc_leg < MIN_BTC_LEG_SAT {
        return err(400, &format!("below the minimum swap ({MIN_BTC_LEG_SAT} sat BTC)"));
    }
    if t.amount_sat > available {
        return err(400, "more than the offer holds");
    }
    if sw.max_btc_sat.is_some_and(|cap| btc_leg > cap) {
        return err(400, "above the maximum swap size");
    }
    let new_offer_id = match new_id() {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    let taker = Offer {
        id: new_offer_id,
        token: token.to_string(),
        secret_hash,
        direction,
        amount_sat: t.amount_sat,
        price_sat_per_xbt: price,
        created_at: now,
        expires_at: now + DEFAULT_EXPIRES_SECS,
        // Never on the public book: it exists only for this take.
        private: true,
        share_code: None,
        status: OfferStatus::Open,
        match_id: None,
        client_id: Some(client_id),
        recovery_tag,
    };
    let m = match matcher::take_public(&sw.store, id, &taker, now, sw.cap(), &sw.network, &sw.fee_address) {
        Ok(m) => m,
        Err(e) => {
            swap_event!(id, "take refused: {e}");
            return err(409, &e.to_string());
        }
    };
    // Whatever the fill left of the taker's side isn't an offer anyone asked for.
    {
        let _g = sw.store.lock_matching();
        if let Some(mut o) = sw.store.offer(&taker.id).filter(|o| o.status == OfferStatus::Open) {
            o.status = OfferStatus::Cancelled;
            if let Err(e) = sw.store.put_offer(&o) {
                return internal(e);
            }
        }
    }
    swap_event!(
        m.id, "listed offer {} taken by offer {}: {} XBT sat for {} BTC sat at {} sat/XBT",
        id, taker.id, m.terms.xbt_sat, m.terms.btc_sat, m.terms.price_sat_per_xbt
    );
    Reply::Json(200, json!({ "match_id": m.id, "owner_secret": secret, "offer_id": taker.id }))
}

fn outpoint_json(o: &Option<(String, u32)>) -> Value {
    match o {
        Some((txid, vout)) => json!({ "txid": txid, "vout": vout }),
        None => Value::Null,
    }
}

/// `POST /swap/recover {tags: [...≤100]}`: for a restored phone. Per known
/// tag: its offer and match (terms, role, status, both fundings) — of an
/// offer in several matches (a remainder taken before it was withdrawn), the
/// one whose own leg is funded. With `claim`, when the tag owner's own leg is
/// funded, its owner secret (offer and that match) is replaced and the new one returned
/// (the restored phone has none; the tag, derived from the seed, proves
/// ownership): the old secret stops working. A tag whose offer was purged is
/// answered `offer_status: "purged"` (so a scan still counts it used).
/// Unknown tags are left out.
fn recover(req: &mut Request, sw: &Swap) -> Reply {
    #[derive(Deserialize)]
    struct RecoverReq {
        tags: Vec<String>,
        /// Take the funded swaps over (new owner secrets). Without it the
        /// call only looks, and changes nothing.
        #[serde(default)]
        claim: bool,
    }
    let r: RecoverReq = match read_json(req) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if r.tags.len() > MAX_RECOVER_TAGS {
        return err(400, &format!("at most {MAX_RECOVER_TAGS} tags"));
    }
    let mut tags = Vec::with_capacity(r.tags.len());
    for t in &r.tags {
        match parse_tag(Some(t)) {
            Ok(Some(t)) => tags.push(t),
            Ok(None) => unreachable!("a tag was given"),
            Err(r) => return r,
        }
    }
    // Under the matching lock: a re-key can't race a match update.
    let _g = sw.store.lock_matching();
    let mut found = Vec::new();
    let own_funded = |m: &Match, offer_id: &str| {
        if m.initiator_offer == offer_id {
            m.xbt_funding.is_some()
        } else {
            m.btc_funding.is_some()
        }
    };
    for tag in tags {
        let hash = tag_hash(&tag);
        if !sw.store.tag_used(&hash) {
            continue;
        }
        let Some(o) = sw.store.offer_by_tag(&hash) else {
            found.push(json!({
                "tag": tag, "offer_id": Value::Null, "offer_status": "purged",
                "direction": Value::Null, "match": Value::Null, "owner_secret": Value::Null,
            }));
            continue;
        };
        let mut owner_secret = Value::Null;
        let mut matched = Value::Null;
        let all = offer_matches(sw, &o.id);
        let chosen = all
            .iter()
            .rev()
            .find(|m| own_funded(m, &o.id))
            .map(|m| m.id.clone())
            .or_else(|| o.match_id.clone())
            .or_else(|| all.last().map(|m| m.id.clone()));
        if let Some(mut m) = chosen.and_then(|id| sw.store.get_match(&id)) {
            let role = if m.initiator_offer == o.id { Role::Initiator } else { Role::Participant };
            let own_funded = own_funded(&m, &o.id);
            if own_funded && r.claim {
                let (secret, hash) = match new_secret() {
                    Ok(s) => s,
                    Err(e) => return internal(e),
                };
                match role {
                    Role::Initiator => m.initiator_secret_hash = hash,
                    Role::Participant => m.participant_secret_hash = hash,
                }
                if let Err(e) = sw.store.put_match(&m).and_then(|_| sw.store.set_secret_hash(&o.id, hash)) {
                    return internal(e);
                }
                swap_event!(m.id, "the {}'s owner secret replaced by recovery", role.as_str());
                owner_secret = json!(secret);
            }
            matched = json!({
                "id": m.id,
                "role": role.as_str(),
                "status": m.status,
                "terms": m.terms,
                "xbt_funding": outpoint_json(&m.xbt_funding),
                "btc_funding": outpoint_json(&m.btc_funding),
            });
        }
        found.push(json!({
            "tag": tag,
            "offer_id": o.id,
            "offer_status": o.status,
            "direction": o.direction,
            "match": matched,
            "owner_secret": owner_secret,
        }));
    }
    eprintln!("fortis-edge: swap recover: {} of {} tags known", found.len(), r.tags.len());
    Reply::Json(200, json!({ "found": found }))
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
    /// `nonce`: the initiator's preimage nonce (16 bytes of hex), relayed so a
    /// phone restored from its seed can rebuild its preimage.
    Hashlock {
        hex: String,
        #[serde(default)]
        nonce: Option<String>,
    },
    Funded { chain: String, txid: String, vout: u32 },
    /// The initiator's unsigned releases of its XBT HTLC (hex), asked of the
    /// participant once the match is cancelled with only the XBT locked.
    ReleaseRequest { txs: Vec<String> },
    /// The participant's signatures on them, one per transaction, in order.
    Release { sigs: Vec<String> },
}

/// Release transactions one request may carry (fee variants).
const MAX_RELEASE_TXS: usize = 4;

/// `txs` as release requests for `xbt_funding`: 1..=4 single-input spends of
/// that outpoint, each at most 1000 bytes, re-encoded as lowercase hex.
fn release_txs(txs: &[String], xbt_funding: &(String, u32)) -> Result<Vec<String>, Reply> {
    if txs.is_empty() || txs.len() > MAX_RELEASE_TXS {
        return Err(err(400, &format!("a release request carries 1 to {MAX_RELEASE_TXS} transactions")));
    }
    let funding = Txid::from_str(&xbt_funding.0).ok().map(|txid| OutPoint { txid, vout: xbt_funding.1 });
    txs.iter()
        .map(|t| {
            let tx = hex::decode(t)
                .ok()
                .filter(|b| b.len() <= 1000)
                .and_then(|b| bitcoin::consensus::deserialize::<Transaction>(&b).ok())
                .ok_or_else(|| err(400, "release transactions must be hex transactions of at most 1000 bytes"))?;
            match tx.input.as_slice() {
                [i] if Some(i.previous_output) == funding => Ok(bitcoin::consensus::encode::serialize_hex(&tx)),
                _ => Err(err(400, "a release spends exactly the match's XBT HTLC")),
            }
        })
        .collect()
}

/// A release signature: DER plus the sighash byte (9..=73 bytes), lowercase hex.
fn release_sig(s: &str) -> Option<String> {
    hex::decode(s).ok().filter(|b| (9..=73).contains(&b.len())).map(hex::encode)
}

/// A v2 match, cancelled, with the initiator's XBT locked and nothing of the participant's.
fn releasable(m: &Match) -> bool {
    m.terms.contract_version >= wallet_core::htlc::CONTRACT_V2
        && m.status == MatchStatus::Cancelled
        && m.xbt_funding.is_some()
        && m.btc_funding.is_none()
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
    let body = match read_small_json(req, MAX_MESSAGE_BODY) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let msg: Msg = match serde_json::from_value(body) {
        Ok(msg) => msg,
        Err(e) => return err(400, &format!("bad message: {e}")),
    };
    // Every message type is set-once or idempotent, so a swap relays about
    // ten; this is the backstop. Checked before any state changes.
    if sw.store.msgs_after(id, 0).len() >= MAX_MESSAGES_PER_MATCH {
        swap_event!(id, "refused a message from the {}: the relay holds {MAX_MESSAGES_PER_MATCH} already", role.as_str());
        return err(429, "this match has relayed too many messages");
    }
    // Relayed as the edge understood it (canonical, no extra fields), never
    // the sender's raw body.
    let pushed = |body: Value| match relay::push(&sw.store, id, role.as_str(), body) {
        Ok(seq) => Reply::Json(200, json!({ "seq": seq })),
        Err(e) => internal(e),
    };
    let unrelayed = || Reply::Json(200, json!({ "seq": Value::Null }));

    match msg {
        Msg::Decline => match matcher::on_decline(&sw.store, id, role) {
            Ok(()) => pushed(json!({ "type": "decline" })),
            Err(e) => err(409, &e.to_string()),
        },
        Msg::Accept => {
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            // A repeat is answered, not relayed again.
            match m.status {
                MatchStatus::Proposed => {}
                MatchStatus::Accepted => return unrelayed(),
                _ => return err(409, "match is past acceptance"),
            }
            let mine = match role {
                Role::Initiator => &mut m.accepted.0,
                Role::Participant => &mut m.accepted.1,
            };
            if *mine {
                return unrelayed();
            }
            if unix_now() > m.terms.accept_by as u64 {
                return err(409, "accept deadline passed");
            }
            *mine = true;
            if m.accepted == (true, true) {
                m.status = MatchStatus::Accepted;
            }
            swap_event!(id, "the {} accepted; status {:?}", role.as_str(), m.status);
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(json!({ "type": "accept" }))
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
                Some(existing) if *existing == (xbt.clone(), btc.clone()) => return unrelayed(),
                Some(_) => return err(409, "different keys are already recorded"),
                None => *slot = Some((xbt.clone(), btc.clone())),
            }
            swap_event!(id, "the {}'s keys recorded", role.as_str());
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(json!({ "type": "keys", "xbt": xbt, "btc": btc }))
        }
        Msg::Hashlock { hex, nonce } => {
            if role != Role::Initiator {
                return err(403, "only the initiator sends the hashlock");
            }
            let Some(h) = hex32(&hex) else {
                return err(400, "hashlock must be 32 bytes of hex");
            };
            let nonce = match nonce.map(|n| hex::decode(n).ok().filter(|b| b.len() == 16)) {
                None => None,
                Some(Some(b)) => Some(hex::encode(b)),
                Some(None) => return err(400, "nonce must be 16 bytes of hex"),
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
                Some(existing) if *existing == h => return unrelayed(),
                Some(_) => return err(409, "a different hashlock is already recorded"),
                None => m.hashlock = Some(h.clone()),
            }
            swap_event!(id, "hashlock recorded");
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            let mut relay = json!({ "type": "hashlock", "hex": h });
            if let Some(n) = nonce {
                relay["nonce"] = json!(n);
            }
            pushed(relay)
        }
        Msg::ReleaseRequest { txs } => {
            if role != Role::Initiator {
                return err(403, "only the initiator asks for a release");
            }
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            if !releasable(&m) {
                return err(409, "a release is only for a cancelled match with only the XBT locked");
            }
            let txs = match release_txs(&txs, m.xbt_funding.as_ref().expect("releasable")) {
                Ok(t) => t,
                Err(r) => return r,
            };
            match &m.release_txs {
                Some(existing) if *existing == txs => return unrelayed(),
                Some(_) => return err(409, "a different release request is already recorded"),
                None => m.release_txs = Some(txs.clone()),
            }
            swap_event!(id, "release requested ({} variants)", txs.len());
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(json!({ "type": "release_request", "txs": txs }))
        }
        Msg::Release { sigs } => {
            if role != Role::Participant {
                return err(403, "only the participant signs a release");
            }
            let Some(sigs) = sigs.iter().map(|s| release_sig(s)).collect::<Option<Vec<_>>>() else {
                return err(400, "release signatures must be DER signatures in hex");
            };
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            if !releasable(&m) {
                return err(409, "a release is only for a cancelled match with only the XBT locked");
            }
            let Some(txs) = &m.release_txs else {
                return err(409, "no release was asked for");
            };
            if sigs.len() != txs.len() {
                return err(400, "one signature per release transaction");
            }
            match &m.release_sigs {
                Some(existing) if *existing == sigs => return unrelayed(),
                Some(_) => return err(409, "a different release is already recorded"),
                None => m.release_sigs = Some(sigs.clone()),
            }
            swap_event!(id, "the participant released the initiator's XBT");
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(json!({ "type": "release", "sigs": sigs }))
        }
        Msg::Funded { chain, txid, vout } => {
            // Released: the participant gave its claim up, so it can't fund (and
            // so reinstate) this match — the initiator may have its XBT back.
            if role == Role::Participant && sw.store.get_match(id).is_some_and(|m| m.release_sigs.is_some()) {
                return err(409, "this match was released: it can't be funded");
            }
            if parse_chain(&chain) != Some(role.send_chain()) {
                return err(400, &format!("the {} funds on {}", role.as_str(), role.send_chain().as_str()));
            }
            let Ok(txid) = Txid::from_str(&txid) else {
                return err(400, "bad txid");
            };
            let outpoint = (txid.to_string(), vout);
            let htlc = OutPoint { txid, vout };
            let relay = json!({ "type": "funded", "chain": role.send_chain().as_str(), "txid": outpoint.0, "vout": vout });
            if role == Role::Participant {
                if let Some(m) = sw.store.get_match(id).filter(|m| m.missed_participant_deadline) {
                    return late_participant_funded(sw, &m, htlc, || pushed(relay));
                }
            }
            let needs = match role {
                Role::Initiator => MatchStatus::Accepted,
                Role::Participant => MatchStatus::InitiatorFunded,
            };
            let recorded = |m: &Match| match role {
                Role::Initiator => m.xbt_funding.clone(),
                Role::Participant => m.btc_funding.clone(),
            };
            // `funded` only names an outpoint: the chain must show it pays this
            // leg's HTLC before the match moves on, or a party could claim a
            // funding it never made (to dodge its deadline and strike, or to
            // lure the other side into funding). Checked before the matching
            // lock (a chain read can be slow). Skipped for the funding already
            // recorded — it was checked when recorded, and is set once — and
            // for a match not at this step, which is refused below anyway: so
            // whatever is recorded under the lock was checked here or before.
            let Some(m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            if recorded(&m) != Some(outpoint.clone()) {
                if m.status != needs {
                    swap_event!(id, "refused the {}'s funded {}:{}: status {:?}", role.as_str(), outpoint.0, outpoint.1, m.status);
                    return err(409, "not your turn to fund");
                }
                if let Err(why) = leg_paid_on_chain(sw, &m, role.send_chain(), &htlc) {
                    swap_event!(id, "refused the {}'s funded {} {}:{}: {}", role.as_str(), chain, outpoint.0, outpoint.1, why.describe());
                    return match why {
                        Unpaid::NoScript => err(409, "send both keys and the hashlock before funding: they are needed to check it"),
                        Unpaid::NoReader => err(503, "fundings can't be checked on chain right now: try again later"),
                        Unpaid::NotSeen => err(503, "this funding isn't seen on chain yet: try again shortly"),
                        Unpaid::Mismatch => err(409, "this output doesn't pay this match's HTLC: wrong script or amount"),
                        Unpaid::ReadFailed(e) => crate::unavailable("the chain", e),
                    };
                }
            }
            let _g = sw.store.lock_matching();
            let Some(mut m) = sw.store.get_match(id) else {
                return err(404, "no such match");
            };
            let (slot, next) = match role {
                Role::Initiator => (&mut m.xbt_funding, MatchStatus::InitiatorFunded),
                Role::Participant => (&mut m.btc_funding, MatchStatus::ParticipantFunded),
            };
            // A refund watch may have recorded this funding first (see
            // `record_leg_funding`); `funded` then still moves the match on.
            match slot {
                Some(existing) if *existing == outpoint && m.status != needs => return unrelayed(),
                Some(existing) if *existing != outpoint => {
                    return err(409, "a different funding is already recorded")
                }
                _ => {}
            }
            if m.status != needs {
                swap_event!(id, "refused the {}'s funded {}:{}: status {:?}", role.as_str(), outpoint.0, outpoint.1, m.status);
                return err(409, "not your turn to fund");
            }
            swap_event!(id, "the {} funded {} {}:{}; status {:?}", role.as_str(), chain, outpoint.0, outpoint.1, next);
            *slot = Some(outpoint);
            m.status = next;
            if let Err(e) = sw.store.put_match(&m) {
                return internal(e);
            }
            pushed(relay)
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
    // The participant's own BTC funding, verified on chain just now: if it was
    // mined in time, a deadline cancel is undone. Never at the refund's
    // expense: the watch below is stored whatever this finds.
    if chain == Chain::Btc && kind == WatchKind::Refund {
        if let Err(e) = reinstate_if_funded_in_time(sw, &m.id) {
            eprintln!("fortis-edge: swap reinstate check for {} failed: {e:#}", m.id);
        }
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
    swap_event!(job.match_id, "the {} handed the watchtower a {} on {} {}:{}", role.as_str(), watchtower::kind_name(&job.kind), job.chain.as_str(), job.htlc.0, job.htlc.1);
    match sw.store.put_watch(&job) {
        Ok(()) => Reply::Json(200, json!({ "ok": true })),
        Err(e) => internal(e),
    }
}

/// The participant's `funded` on a match the deadline tick cancelled: the
/// funding is checked on chain exactly as a refund watch's is (this leg's HTLC
/// script and amount), recorded, and — if it was mined by
/// `participant_fund_by` — the match is reinstated and the `funded` relayed.
/// A funding mined late (or not mined yet) is refused with 409; its refund
/// watch still works, and a later `funded` can try again once it is mined.
fn late_participant_funded(sw: &Swap, m: &Match, htlc: OutPoint, relay: impl FnOnce() -> Reply) -> Reply {
    if let Err(r) = record_leg_funding(sw, m, Chain::Btc, &htlc, true) {
        return r;
    }
    match reinstate_if_funded_in_time(sw, &m.id) {
        Ok(Reinstate::Done) => relay(),
        Ok(Reinstate::MinedLate) => {
            swap_event!(m.id, "refused the participant's late funded {htlc}: mined after its deadline (final)");
            err(410, "the match was cancelled: this funding was mined after its deadline")
        }
        Ok(Reinstate::NotMined) => {
            swap_event!(m.id, "refused the participant's late funded {htlc}: not mined yet");
            err(409, "the match was cancelled: this funding isn't mined yet")
        }
        // Reinstated meanwhile (its refund watch got there first): a no-op.
        Ok(Reinstate::NotEligible)
            if sw.store.get_match(&m.id).is_some_and(|cur| {
                cur.status == MatchStatus::ParticipantFunded
                    && cur.btc_funding == Some((htlc.txid.to_string(), htlc.vout))
            }) =>
        {
            Reply::Json(200, json!({ "seq": Value::Null }))
        }
        Ok(Reinstate::NotEligible) => err(409, "the match was cancelled"),
        Err(e) => crate::unavailable("the chain", e),
    }
}

/// What [`reinstate_if_funded_in_time`] found.
#[derive(Debug, PartialEq)]
enum Reinstate {
    /// Reinstated by this call.
    Done,
    /// The recorded funding isn't mined yet: it may still be in time.
    NotMined,
    /// Mined after the deadline: final.
    MinedLate,
    /// Not a deadline-cancelled match with a recorded BTC funding (any more).
    NotEligible,
}

/// Undo a deadline cancel whose BTC funding was in fact mined in time. The
/// deadline is about funding, not about the edge hearing of it: a phone that
/// funded on time but couldn't announce it (it couldn't see its own funding,
/// or couldn't reach the edge) must not lose the swap, or be struck for it.
///
/// Only a match cancelled for [`Match::missed_participant_deadline`], whose
/// recorded `btc_funding` (verified on chain by [`record_leg_funding`]) was
/// mined at or before `participant_fund_by`, is reinstated: status
/// `ParticipantFunded`, not closed, the participant's strike lifted. Its
/// offers stay cancelled — off the book; the match carries the swap.
fn reinstate_if_funded_in_time(sw: &Swap, id: &str) -> anyhow::Result<Reinstate> {
    // Never a released match: the initiator may have its XBT back already.
    let eligible = |m: &Match| {
        m.missed_participant_deadline && m.status == MatchStatus::Cancelled && m.release_sigs.is_none()
    };
    let Some(m) = sw.store.get_match(id).filter(eligible) else {
        return Ok(Reinstate::NotEligible);
    };
    let (Some((txid, _)), Some(fund_by)) = (m.btc_funding.clone(), m.participant_fund_by) else {
        return Ok(Reinstate::NotEligible);
    };
    let Some(reader) = &sw.chain else {
        return Ok(Reinstate::NotEligible);
    };
    // Read without the matching lock; re-checked under it below.
    let Some(mined) = reader.tx_block_time(Chain::Btc, &txid)? else {
        return Ok(Reinstate::NotMined);
    };
    if mined > fund_by as u64 {
        swap_event!(id, "not reinstated: BTC funding {txid} was mined at {mined}, after the deadline {fund_by}");
        return Ok(Reinstate::MinedLate);
    }

    let _g = sw.store.lock_matching();
    let Some(mut cur) = sw.store.get_match(id).filter(eligible) else {
        return Ok(Reinstate::NotEligible);
    };
    if cur.btc_funding.as_ref().map(|f| &f.0) != Some(&txid) {
        return Ok(Reinstate::NotEligible);
    }
    // The strike first: should the match write then fail, a retry still
    // reinstates it (and finds no strike left to lift).
    if let Some(at) = cur.closed_at {
        sw.store.remove_strike(&cur.participant_token, at)?;
    }
    cur.status = MatchStatus::ParticipantFunded;
    cur.missed_participant_deadline = false;
    cur.closed_at = None;
    sw.store.put_match(&cur)?;
    swap_event!(id, "reinstated: BTC funding {txid} was mined at {mined}, by the deadline {fund_by}; the participant's strike lifted");
    Ok(Reinstate::Done)
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
    match leg_paid_on_chain(sw, m, chain, htlc) {
        Ok(()) => {}
        Err(Unpaid::NoScript) => {
            return Err(err(409, "htlc_outpoint is not recorded, and both keys and the hashlock are needed to check it"))
        }
        Err(Unpaid::ReadFailed(e)) => return Err(crate::unavailable("the chain", e)),
        Err(Unpaid::NoReader | Unpaid::NotSeen | Unpaid::Mismatch) => {
            return Err(err(409, "htlc_outpoint is not this match's HTLC on that chain"))
        }
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

/// Why a claimed funding is not (or not yet) shown to be the leg's HTLC.
enum Unpaid {
    /// Keys and hashlock aren't all relayed: there is no script to check.
    NoScript,
    /// The edge runs without a chain reader.
    NoReader,
    /// The chain doesn't show the transaction (or that output) yet.
    NotSeen,
    /// The output exists but pays another script or another amount.
    Mismatch,
    /// The chain couldn't be read.
    ReadFailed(anyhow::Error),
}

impl Unpaid {
    fn describe(&self) -> String {
        match self {
            Unpaid::NoScript => "keys or hashlock not relayed yet".into(),
            Unpaid::NoReader => "no chain reader".into(),
            Unpaid::NotSeen => "not seen on chain".into(),
            Unpaid::Mismatch => "not this leg's HTLC script and amount".into(),
            Unpaid::ReadFailed(e) => format!("chain read failed: {e:#}"),
        }
    }
}

/// Whether `htlc` — the output as its transaction created it, spent or not
/// (a late announcement of a funding already redeemed still counts) — pays
/// exactly `m`'s HTLC script on `chain` (from both relayed keys and the
/// hashlock) and exactly the leg's amount. Reads the chain: call it without
/// `lock_matching()` held.
fn leg_paid_on_chain(sw: &Swap, m: &Match, chain: Chain, htlc: &OutPoint) -> Result<(), Unpaid> {
    let ws = leg_witness_script(m, chain).ok_or(Unpaid::NoScript)?;
    let reader = sw.chain.as_ref().ok_or(Unpaid::NoReader)?;
    let out = reader
        .created_output(chain, &htlc.txid.to_string(), htlc.vout)
        .map_err(Unpaid::ReadFailed)?
        .ok_or(Unpaid::NotSeen)?;
    let spk = ScriptBuf::new_p2wsh(&ws.wscript_hash());
    let amount = match chain {
        Chain::Xbt => m.terms.xbt_sat,
        Chain::Btc => m.terms.btc_sat,
    };
    if !out.scriptpubkey_hex.eq_ignore_ascii_case(&hex::encode(spk.as_bytes())) || out.value_sat != amount {
        return Err(Unpaid::Mismatch);
    }
    Ok(())
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
        // the match's own contract version: a v1 match keeps the v1 script
        Chain::Xbt => HtlcContract::try_build(
            &xbt_params,
            m.terms.contract_version,
            hashlock,
            &key(p_xbt)?,
            &key(i_xbt)?,
            m.terms.xbt_locktime,
            Amount::from_sat(m.terms.xbt_sat),
        )
        .ok()?,
        Chain::Btc => HtlcContract::try_build(
            &btc_params,
            m.terms.contract_version,
            hashlock,
            &key(i_btc)?,
            &key(p_btc)?,
            m.terms.btc_locktime,
            Amount::from_sat(m.terms.btc_sat),
        )
        .ok()?,
    };
    Some(c.witness_script)
}

/// True only if `raw_tx_hex` decodes, has exactly **one** input, and it spends
/// an outpoint recorded as a match's funding on `chain` (the chain it is broadcast to)
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
    // Every HTLC spend — redeem, refund, the watchtower's variants — has
    // exactly one input. Anything else isn't one, and is checked no further:
    // a many-input transaction can't make this scan the matches per input.
    let [i] = tx.input.as_slice() else {
        return false;
    };
    let o = i.previous_output;
    // Several matches may claim one outpoint (a squatter can name a victim's
    // HTLC); the spend counts if it matches any of them.
    sw.store
        .matches_funded_by(chain, &o.txid.to_string(), o.vout)
        .iter()
        .filter_map(|m| leg_witness_script(m, chain))
        .any(|ws| i.witness.last() == Some(ws.as_bytes()))
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
        new_swap_at(neoxex_stub(fresh))
    }

    fn new_swap_at(neoxex_url: String) -> (Swap, crate::test_tmp::TempDir) {
        new_swap_with(neoxex_url, Some(1_000_000))
    }

    fn new_swap_capped(cap: Option<u64>) -> (Swap, crate::test_tmp::TempDir) {
        new_swap_with(neoxex_stub(true), cap)
    }

    fn new_swap_with(neoxex_url: String, cap: Option<u64>) -> (Swap, crate::test_tmp::TempDir) {
        let dir = temp_dir("db");
        let sw = Swap::new(
            Store::open(&dir).unwrap(),
            NeoxEx::new(neoxex_url),
            50,
            cap,
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
        /// The edge's chain reader (`funded` and watches check HTLCs on it).
        chain: Arc<StubChain>,
        _db: crate::test_tmp::TempDir,
    }

    fn edge_with((sw, db): (Swap, crate::test_tmp::TempDir)) -> Edge {
        let chain = Arc::new(StubChain::default());
        let sw = Arc::new(sw.with_chain_reader(chain.clone()));
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
        Edge { url, sw, chain, _db: db }
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
            self.priced(token, direction, amount_sat, PRICE, private)
        }

        fn priced(
            &self,
            token: &str,
            direction: &str,
            amount_sat: u64,
            price: u64,
            private: bool,
        ) -> (u16, Value) {
            self.post(
                "/swap/offers",
                token,
                None,
                json!({ "direction": direction, "amount_sat": amount_sat,
                        "price_sat_per_xbt": price, "private": private }),
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
    fn quote_returns_reference_and_limits() {
        let e = edge();
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 200, "{q}");
        assert_eq!(q["reference_price"], PRICE);
        assert_eq!(q["reference_ok"], true);
        assert_eq!(q["best"], json!({ "xbt_to_btc": null, "btc_to_xbt": null }));
        assert_eq!(q["fee_bps"], 50);
        assert_eq!(q["min_btc_sat"], 2_000);
        assert_eq!(q["max_btc_sat"], 1_000_000);
        assert_eq!(q["open"], json!({ "xbt_to_btc": 0, "btc_to_xbt": 0 }));
        assert!(q.get("price_sat_per_xbt").is_none(), "{q}");

        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, false).0, 200);
        // Private offers aren't on the public book.
        assert_eq!(e.priced("tok-a", "btc_to_xbt", 200_000, PRICE + 5, true).0, 200);
        let (_, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(q["open"], json!({ "xbt_to_btc": 0, "btc_to_xbt": 1 }));
        assert_eq!(q["best"], json!({ "xbt_to_btc": null, "btc_to_xbt": PRICE }));
    }

    #[test]
    fn quote_works_when_neoxex_refuses() {
        let e = edge_with(new_swap(false));
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 200, "{q}");
        assert_eq!(q["reference_ok"], false);
        // Stale, but still a reference.
        assert_eq!(q["reference_price"], PRICE);
        // Posting doesn't depend on NeoxEX.
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, false).0, 200);
    }

    #[test]
    fn quote_reference_null_when_neoxex_down() {
        // Nothing listens on port 1.
        let e = edge_with(new_swap_at("http://127.0.0.1:1".into()));
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 200, "{q}");
        assert!(q["reference_price"].is_null(), "{q}");
        assert_eq!(q["reference_ok"], false);
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 200_000, false).0, 200);
    }

    #[test]
    fn quote_reports_best_open_prices() {
        let e = edge();
        for (dir, amount, price, private) in [
            ("xbt_to_btc", 20_000_000, 1_000_000, false),
            ("xbt_to_btc", 20_000_000, 990_000, false),
            ("btc_to_xbt", 200_000, 900_000, false),
            // Off the public book: not a best price.
            ("xbt_to_btc", 20_000_000, 950_000, true),
            ("btc_to_xbt", 200_000, 920_000, true),
        ] {
            let (s, o) = e.priced("tok-a", dir, amount, price, private);
            assert_eq!(s, 200, "{o}");
            assert!(o["match_id"].is_null(), "{o}");
        }
        let (_, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(q["best"], json!({ "xbt_to_btc": 990_000, "btc_to_xbt": 900_000 }));
        assert_eq!(q["open"], json!({ "xbt_to_btc": 2, "btc_to_xbt": 1 }));
    }

    // ---- GET /swap/book ----

    fn book(e: &Edge, token: &str) -> Value {
        let (s, b) = e.get("/swap/book", token, None);
        assert_eq!(s, 200, "{b}");
        b
    }

    #[test]
    fn the_book_lists_open_public_offers_best_price_first() {
        let e = edge();
        for (dir, amount, price) in [
            ("xbt_to_btc", 20_000_000, 1_000_000),
            ("xbt_to_btc", 20_000_000, 990_000),
            ("xbt_to_btc", 30_000_000, 990_000), // same price, newer: after
            ("btc_to_xbt", 200_000, 900_000),
            ("btc_to_xbt", 300_000, 950_000),
        ] {
            let (s, o) = e.priced("tok-a", dir, amount, price, false);
            assert_eq!(s, 200, "{o}");
            if amount == 30_000_000 {
                // Posted in the same second as the other 990 000 ask: make it newer.
                let mut stored = e.sw.store.offer(o["offer_id"].as_str().unwrap()).unwrap();
                stored.created_at += 1;
                e.sw.store.put_offer(&stored).unwrap();
            }
        }
        let b = book(&e, "tok-viewer");
        let asks: Vec<(u64, u64)> = b["xbt_to_btc"].as_array().unwrap().iter()
            .map(|x| (x["price_sat_per_xbt"].as_u64().unwrap(), x["amount_sat"].as_u64().unwrap())).collect();
        assert_eq!(asks, vec![(990_000, 20_000_000), (990_000, 30_000_000), (1_000_000, 20_000_000)]);
        let bids: Vec<u64> = b["btc_to_xbt"].as_array().unwrap().iter()
            .map(|x| x["price_sat_per_xbt"].as_u64().unwrap()).collect();
        assert_eq!(bids, vec![950_000, 900_000]);
        // Both legs of the open amount, at the offer's price.
        let a = &b["xbt_to_btc"][0];
        assert_eq!(a["xbt_sat"], 20_000_000);
        assert_eq!(a["btc_sat"], btc_leg_value(Direction::XbtToBtc, 20_000_000, 990_000));
        let bid = &b["btc_to_xbt"][0];
        assert_eq!(bid["btc_sat"], 300_000);
        assert_eq!(bid["xbt_sat"], xbt_for_btc(300_000, 950_000));
        assert_eq!(a["direction"], "xbt_to_btc");
        assert!(a["id"].is_string() && a["expires_at"].is_u64());
    }

    #[test]
    fn the_book_leaves_out_private_own_struck_expired_and_tiny_offers() {
        let e = edge();
        let (_, private) = e.priced("tok-a", "xbt_to_btc", 20_000_000, 990_000, true);
        let (_, own) = e.priced("tok-viewer", "xbt_to_btc", 20_000_000, 990_000, false);
        let (_, struck) = e.priced("tok-struck", "xbt_to_btc", 20_000_000, 990_000, false);
        for _ in 0..MAX_STRIKES {
            e.sw.store.add_strike("tok-struck", unix_now()).unwrap();
        }
        let (_, expired) = e.priced("tok-a", "xbt_to_btc", 20_000_000, 990_000, false);
        let mut o = e.sw.store.offer(expired["offer_id"].as_str().unwrap()).unwrap();
        o.expires_at = unix_now() - 1;
        e.sw.store.put_offer(&o).unwrap();
        let (_, tiny) = e.priced("tok-a", "xbt_to_btc", 20_000_000, 990_000, false);
        let mut o = e.sw.store.offer(tiny["offer_id"].as_str().unwrap()).unwrap();
        o.amount_sat = 100_000; // its BTC leg is below the swap minimum now
        e.sw.store.put_offer(&o).unwrap();
        let (_, listed) = e.priced("tok-a", "xbt_to_btc", 20_000_000, 990_000, false);

        let ids: Vec<String> = book(&e, "tok-viewer")["xbt_to_btc"].as_array().unwrap().iter()
            .map(|x| x["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, vec![listed["offer_id"].as_str().unwrap().to_string()]);
        for gone in [&private, &own, &struck, &expired, &tiny] {
            assert!(!ids.contains(&gone["offer_id"].as_str().unwrap().to_string()));
        }
        // The owner of `own` doesn't see it; anyone else does.
        let others: usize = book(&e, "tok-someone")["xbt_to_btc"].as_array().unwrap().len();
        assert_eq!(others, 2, "listed + own");
    }

    #[test]
    fn the_book_caps_each_side_at_50() {
        let e = edge();
        for i in 0..55u64 {
            // spread over installs: each may hold at most MAX_OPEN_OFFERS
            let token = format!("tok-{}", i as usize / MAX_OPEN_OFFERS);
            let (s, o) = e.priced(&token, "xbt_to_btc", 20_000_000, 990_000 + i, false);
            assert_eq!(s, 200, "{o}");
        }
        let b = book(&e, "tok-viewer");
        let asks = b["xbt_to_btc"].as_array().unwrap();
        assert_eq!(asks.len(), BOOK_MAX_PER_SIDE);
        assert_eq!(asks[0]["price_sat_per_xbt"], 990_000, "the best 50 are kept");
    }

    #[test]
    fn book_entries_carry_no_secrets() {
        let e = edge();
        let (_, o) = e.post("/swap/offers", "tok-secret-owner", None, json!({
            "direction": "xbt_to_btc", "amount_sat": 20_000_000, "price_sat_per_xbt": 990_000,
            "private": false, "client_id": "c0ffee" }));
        let stored = e.sw.store.offer(o["offer_id"].as_str().unwrap()).unwrap();
        let text = book(&e, "tok-viewer").to_string();
        assert!(text.contains(&stored.id));
        for secret in [stored.token.clone(), hex::encode(stored.secret_hash), "c0ffee".into(),
                       o["owner_secret"].as_str().unwrap().to_string()] {
            assert!(!text.contains(&secret), "{secret} leaked");
        }
        let entry = &book(&e, "tok-viewer")["xbt_to_btc"][0];
        let keys: std::collections::BTreeSet<&str> = entry.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["amount_sat", "btc_sat", "direction", "expires_at", "id", "price_sat_per_xbt", "xbt_sat"].into());
    }

    // ---- recovery tags and POST /swap/recover ----

    fn rtag(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn tagged_offer(e: &Edge, token: &str, direction: &str, amount: u64, tag: &str, client: &str) -> (u16, Value) {
        e.post(
            "/swap/offers",
            token,
            None,
            json!({ "direction": direction, "amount_sat": amount, "price_sat_per_xbt": PRICE,
                    "private": false, "client_id": client, "recovery_tag": tag }),
        )
    }

    fn recover(e: &Edge, tags: &[String]) -> (u16, Value) {
        e.post("/swap/recover", "tok-restored", None, json!({ "tags": tags, "claim": true }))
    }

    /// The recover entry for `tag`, if the reply has one.
    fn found<'a>(r: &'a Value, tag: &str) -> Option<&'a Value> {
        r["found"].as_array().unwrap().iter().find(|f| f["tag"] == tag)
    }

    #[test]
    fn a_used_tag_is_refused_but_a_replay_is_not() {
        let e = edge();
        let (s, a) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, &rtag(5), "c1");
        assert_eq!(s, 200, "{a}");
        let (s, again) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, &rtag(5), "c1");
        assert_eq!((s, &again["offer_id"]), (200, &a["offer_id"]), "a replay is answered, not refused");
        for (token, client) in [("tok-a", "c2"), ("tok-b", "c1")] {
            let (s, r) = tagged_offer(&e, token, "btc_to_xbt", 200_000, &rtag(5), client);
            assert_eq!(s, 409, "{r}");
            assert_eq!(r["error"], "this swap index was used before");
        }
        let (s, r) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, "not-hex", "c3");
        assert_eq!(s, 400, "{r}");
    }

    #[test]
    fn a_take_carries_its_tag() {
        let e = edge();
        let listed = ask(&e, 20_000_000, PRICE);
        let btc = btc_leg_value(Direction::XbtToBtc, 20_000_000, PRICE);
        let (s, t) = e.post(
            &format!("/swap/offers/{listed}/take"),
            "tok-taker",
            None,
            json!({ "amount_sat": btc, "price_sat_per_xbt": PRICE, "client_id": "aa02", "recovery_tag": rtag(7) }),
        );
        assert_eq!(s, 200, "{t}");
        assert_eq!(e.sw.store.offer_by_tag(&tag_hash(&rtag(7))).unwrap().id, t["offer_id"].as_str().unwrap());

        let (s, p) = e.priced("tok-maker2", "btc_to_xbt", 200_000, PRICE, true);
        assert_eq!(s, 200, "{p}");
        let code = p["share_code"].as_str().unwrap();
        let (s, t) = e.post(&format!("/swap/take/{code}"), "tok-c", None, json!({ "recovery_tag": rtag(8) }));
        assert_eq!(s, 200, "{t}");
        assert_eq!(e.sw.store.offer_by_tag(&tag_hash(&rtag(8))).unwrap().id, t["offer_id"].as_str().unwrap());
        // A tag used before is refused on a take too.
        let (s, p) = e.priced("tok-maker3", "btc_to_xbt", 200_000, PRICE, true);
        assert_eq!(s, 200, "{p}");
        let code = p["share_code"].as_str().unwrap();
        assert_eq!(e.post(&format!("/swap/take/{code}"), "tok-d", None, json!({ "recovery_tag": rtag(8) })).0, 409);
    }

    #[test]
    fn recover_answers_only_known_tags_and_funded_secrets() {
        let e = edge();
        let (s, part) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, &rtag(2), "c2");
        assert_eq!(s, 200, "{part}");
        let (s, init) = tagged_offer(&e, "tok-b", "xbt_to_btc", 20_000_000, &rtag(1), "c1");
        assert_eq!(s, 200, "{init}");
        let id = init["match_id"].as_str().expect("matched").to_string();
        let (init_secret, part_secret) =
            (init["owner_secret"].as_str().unwrap().to_string(), part["owner_secret"].as_str().unwrap().to_string());

        // Matched, nothing funded: known tags answered, no secrets; unknown ones absent.
        let (s, r) = recover(&e, &[rtag(1), rtag(2), rtag(9)]);
        assert_eq!(s, 200, "{r}");
        assert!(found(&r, &rtag(9)).is_none());
        let f1 = found(&r, &rtag(1)).unwrap();
        assert_eq!(f1["offer_id"], init["offer_id"]);
        assert_eq!(f1["match"]["id"], id.as_str());
        assert_eq!(f1["match"]["role"], "initiator");
        assert_eq!(f1["owner_secret"], Value::Null);
        assert_eq!(found(&r, &rtag(2)).unwrap()["match"]["role"], "participant");

        // The initiator funds its XBT leg: its tag now re-keys its side.
        assert_eq!(e.msg(&id, &init_secret, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part_secret, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(&e, &id, &init_secret, &part_secret);
        let t = terms_of(&e, &id, &init_secret);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        e.chain.put(Chain::Xbt, htlc, t.xbt_sat, &xbt_leg(&t, &i, &p).script_pubkey);
        assert_eq!(e.msg(&id, &init_secret, funded_msg("xbt", htlc)).0, 200);

        let (_, r) = recover(&e, &[rtag(1), rtag(2)]);
        let f1 = found(&r, &rtag(1)).unwrap();
        assert_eq!(f1["match"]["xbt_funding"], json!({ "txid": htlc.txid.to_string(), "vout": 1 }));
        let fresh = f1["owner_secret"].as_str().expect("a fresh secret for a funded own leg").to_string();
        assert_eq!(found(&r, &rtag(2)).unwrap()["owner_secret"], Value::Null, "the participant's leg isn't funded");
        let (s, m) = e.get(&format!("/swap/matches/{id}"), "tok", Some(&fresh));
        assert_eq!((s, &m["role"]), (200, &json!("initiator")), "{m}");
        assert_eq!(e.get(&format!("/swap/matches/{id}"), "tok", Some(&init_secret)).0, 403, "the old secret no longer works");
        assert_eq!(e.get(&format!("/swap/offers/{}", init["offer_id"].as_str().unwrap()), "tok", Some(&fresh)).0, 200);
    }

    /// Without `claim` (a scan that only looks: catching a counter up, or
    /// finding which swaps to take over) nothing is re-keyed and no secret is
    /// handed out, so a phone still running the swap keeps its secret.
    #[test]
    fn recover_without_claim_rekeys_nothing() {
        let e = edge();
        let (_, part) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, &rtag(2), "c2");
        let (_, init) = tagged_offer(&e, "tok-b", "xbt_to_btc", 20_000_000, &rtag(1), "c1");
        let id = init["match_id"].as_str().unwrap().to_string();
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.xbt_funding = Some((txid(0xaa).to_string(), 1));
        e.sw.store.put_match(&m).unwrap();
        let _ = part;
        let (s, r) = e.post("/swap/recover", "tok-restored", None, json!({ "tags": [rtag(1)] }));
        assert_eq!(s, 200, "{r}");
        let f = found(&r, &rtag(1)).unwrap();
        assert_eq!(f["owner_secret"], Value::Null);
        assert_eq!(f["match"]["xbt_funding"]["vout"], 1, "the funding is reported");
        let old = init["owner_secret"].as_str().unwrap();
        assert_eq!(e.get(&format!("/swap/matches/{id}"), "tok", Some(old)).0, 200, "the old secret still works");
    }

    /// A purged offer's tag is still used (a swap index is used once, ever) and
    /// still known to a restore scan, so its counter moves past it.
    #[test]
    fn a_tag_stays_used_and_known_after_its_offer_is_purged() {
        let e = edge();
        let (s, o) = tagged_offer(&e, "tok-a", "xbt_to_btc", 20_000_000, &rtag(3), "c3");
        assert_eq!(s, 200, "{o}");
        let id = o["offer_id"].as_str().unwrap();
        let mut stored = e.sw.store.offer(id).unwrap();
        stored.status = OfferStatus::Cancelled;
        e.sw.store.put_offer(&stored).unwrap();
        e.sw.store.purge(stored.expires_at + store::PURGE_AFTER_SECS + 1);
        assert!(e.sw.store.offer(id).is_none(), "purged");

        let (s, r) = tagged_offer(&e, "tok-b", "xbt_to_btc", 20_000_000, &rtag(3), "c4");
        assert_eq!((s, r["error"].as_str()), (409, Some(TAG_USED)), "{r}");
        let (_, r) = recover(&e, &[rtag(3)]);
        let f = found(&r, &rtag(3)).expect("a purged tag is still known");
        assert_eq!(f["offer_status"], "purged");
        assert_eq!((&f["match"], &f["owner_secret"]), (&Value::Null, &Value::Null));
    }

    /// An offer stays while a match of it does, so a funded swap whose
    /// watch job outlives the offer's retention can still be recovered.
    #[test]
    fn an_offer_is_kept_while_its_match_is() {
        let e = edge();
        let (_, part) = tagged_offer(&e, "tok-a", "btc_to_xbt", 200_000, &rtag(2), "c2");
        let (_, init) = tagged_offer(&e, "tok-b", "xbt_to_btc", 20_000_000, &rtag(1), "c1");
        assert!(init["match_id"].is_string(), "{init}");
        for id in [init["offer_id"].as_str().unwrap(), part["offer_id"].as_str().unwrap()] {
            let mut o = e.sw.store.offer(id).unwrap();
            o.status = OfferStatus::Filled;
            e.sw.store.put_offer(&o).unwrap();
        }
        let o = e.sw.store.offer(init["offer_id"].as_str().unwrap()).unwrap();
        e.sw.store.purge(o.expires_at + store::PURGE_AFTER_SECS + 1);
        assert!(e.sw.store.offer(init["offer_id"].as_str().unwrap()).is_some(), "its match is still there");
        assert!(e.sw.store.offer(part["offer_id"].as_str().unwrap()).is_some());
    }

    /// A partly filled offer can be in two matches (the remainder was taken
    /// before the owner withdrew it): recovery follows — and re-keys — the one
    /// whose own leg is funded, never a later unfunded one.
    #[test]
    fn recover_follows_the_funded_match_of_a_partly_filled_offer() {
        let e = edge();
        let (s, part) = tagged_offer(&e, "tok-a", "btc_to_xbt", 400_000, &rtag(2), "c2");
        assert_eq!(s, 200, "{part}");
        let (_, i1) = tagged_offer(&e, "tok-b", "xbt_to_btc", 20_000_000, &rtag(1), "c1");
        let m1 = i1["match_id"].as_str().expect("matched").to_string();
        let mut m = e.sw.store.get_match(&m1).unwrap();
        m.btc_funding = Some((txid(0xbb).to_string(), 0));
        e.sw.store.put_match(&m).unwrap();
        let (_, i2) = tagged_offer(&e, "tok-c", "xbt_to_btc", 20_000_000, &rtag(5), "c5");
        let m2 = i2["match_id"].as_str().expect("the remainder matched").to_string();
        assert_ne!(m1, m2);
        let m2_hash = e.sw.store.get_match(&m2).unwrap().participant_secret_hash;

        let (_, r) = recover(&e, &[rtag(2)]);
        let f = found(&r, &rtag(2)).unwrap();
        assert_eq!(f["match"]["id"], m1.as_str(), "{r}");
        let fresh = f["owner_secret"].as_str().expect("its leg in m1 is funded");
        assert_eq!(e.get(&format!("/swap/matches/{m1}"), "tok", Some(fresh)).0, 200);
        assert_eq!(e.sw.store.get_match(&m2).unwrap().participant_secret_hash, m2_hash, "m2 untouched");
    }

    /// The edge keeps SHA-256 of each tag, never the tag: a leaked database
    /// gives nobody the tags that re-key a swap through `/swap/recover`.
    #[test]
    fn the_store_keeps_tag_hashes_not_tags() {
        let e = edge();
        let (_, o) = tagged_offer(&e, "tok-a", "xbt_to_btc", 20_000_000, &rtag(7), "c7");
        let stored = e.sw.store.offer(o["offer_id"].as_str().unwrap()).unwrap();
        assert!(!serde_json::to_string(&stored).unwrap().contains(&rtag(7)));
        assert!(!e.sw.store.tag_used(&rtag(7)), "the store is keyed by the hash");
        assert!(e.sw.store.tag_used(&tag_hash(&rtag(7))));
    }

    /// Two offers can't end up with one tag, even racing past the route's
    /// early check: the store refuses the second.
    #[test]
    fn the_store_refuses_a_second_offer_with_a_used_tag() {
        let e = edge();
        let (_, o) = tagged_offer(&e, "tok-a", "xbt_to_btc", 20_000_000, &rtag(7), "c7");
        let mut other = e.sw.store.offer(o["offer_id"].as_str().unwrap()).unwrap();
        other.id = "another".into();
        let err = e.sw.store.put_offer(&other).unwrap_err();
        assert!(err.to_string().contains(TAG_USED), "{err}");
        // the offer itself re-saves (status updates) as before
        let mine = e.sw.store.offer(o["offer_id"].as_str().unwrap()).unwrap();
        e.sw.store.put_offer(&mine).unwrap();
    }

    #[test]
    fn recover_takes_at_most_100_tags_and_only_hex_ones() {
        let e = edge();
        let many: Vec<String> = (0..=100).map(|n| format!("{:064x}", n)).collect();
        assert_eq!(recover(&e, &many).0, 400);
        assert_eq!(recover(&e, &many[..100]).0, 200);
        assert_eq!(recover(&e, &["nothex".to_string()]).0, 400);
    }

    // ---- POST /swap/offers/{id}/take ----

    fn take(e: &Edge, token: &str, id: &str, amount: u64, price: u64, client: &str) -> (u16, Value) {
        e.post(
            &format!("/swap/offers/{id}/take"),
            token,
            None,
            json!({ "amount_sat": amount, "price_sat_per_xbt": price, "client_id": client }),
        )
    }

    /// An ask of `amount` XBT sat at `price` from tok-maker; its offer id.
    fn ask(e: &Edge, amount: u64, price: u64) -> String {
        let (s, o) = e.priced("tok-maker", "xbt_to_btc", amount, price, false);
        assert_eq!(s, 200, "{o}");
        assert!(o["match_id"].is_null());
        o["offer_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn a_take_pairs_exactly_the_chosen_offer_at_its_price() {
        let e = edge();
        let better = ask(&e, 20_000_000, 980_000);
        let chosen = ask(&e, 20_000_000, 990_000);
        let btc = btc_leg_value(Direction::XbtToBtc, 20_000_000, 990_000);
        let (s, t) = take(&e, "tok-taker", &chosen, btc, 990_000, "aa01");
        assert_eq!(s, 200, "{t}");
        let m = e.sw.store.get_match(t["match_id"].as_str().unwrap()).unwrap();
        assert_eq!(m.initiator_offer, chosen);
        assert_eq!(m.terms.price_sat_per_xbt, 990_000);
        assert_eq!(m.participant_offer, t["offer_id"].as_str().unwrap());
        // The better-priced offer is untouched and still listed.
        assert_eq!(e.sw.store.offer(&better).unwrap().status, OfferStatus::Open);
        assert!(t["owner_secret"].is_string());
        // The taker's own side is never on the book.
        assert!(e.sw.store.offer(t["offer_id"].as_str().unwrap()).unwrap().private);
    }

    #[test]
    fn a_partial_take_leaves_the_rest_listed() {
        let e = edge();
        let chosen = ask(&e, 40_000_000, 1_000_000);
        // Take the BTC worth of a quarter of it.
        let (s, t) = take(&e, "tok-taker", &chosen, 100_000, 1_000_000, "aa02");
        assert_eq!(s, 200, "{t}");
        let m = e.sw.store.get_match(t["match_id"].as_str().unwrap()).unwrap();
        assert_eq!(m.terms.btc_sat, 100_000);
        let listed = &book(&e, "tok-viewer")["xbt_to_btc"][0];
        assert_eq!(listed["id"], chosen.as_str());
        assert_eq!(listed["amount_sat"], 40_000_000 - m.terms.xbt_sat);
        // A remainder below the minimum leaves the book.
        let small = ask(&e, 1_000_000, 1_000_000); // 10 000 sat BTC
        let (s, _) = take(&e, "tok-taker", &small, 9_000, 1_000_000, "aa03");
        assert_eq!(s, 200);
        let ids: Vec<String> = book(&e, "tok-viewer")["xbt_to_btc"].as_array().unwrap().iter()
            .map(|x| x["id"].as_str().unwrap().into()).collect();
        assert!(!ids.contains(&small), "a 1 000 sat remainder isn't takeable");
    }

    #[test]
    fn a_repeat_take_with_the_same_client_id_takes_once() {
        let e = edge();
        let chosen = ask(&e, 40_000_000, 1_000_000);
        let first = take(&e, "tok-taker", &chosen, 100_000, 1_000_000, "aa04");
        let again = take(&e, "tok-taker", &chosen, 100_000, 1_000_000, "aa04");
        assert_eq!(first.0, 200, "{}", first.1);
        assert_eq!(first, again);
        let matches = e.sw.store.all_matches().into_iter().filter(|m| m.initiator_offer == chosen).count();
        assert_eq!(matches, 1);
    }

    #[test]
    fn a_take_with_a_stale_price_is_refused() {
        let e = edge();
        let chosen = ask(&e, 20_000_000, 990_000);
        let (s, r) = take(&e, "tok-taker", &chosen, 100_000, 980_000, "aa05");
        assert_eq!(s, 409, "{r}");
        assert!(r["error"].as_str().unwrap().contains("price changed"));
        assert!(e.sw.store.all_matches().is_empty());
    }

    #[test]
    fn taking_a_gone_offer_is_refused_and_creates_nothing() {
        let e = edge();
        // filled
        let filled = ask(&e, 20_000_000, 990_000);
        let full = btc_leg_value(Direction::XbtToBtc, 20_000_000, 990_000);
        assert_eq!(take(&e, "tok-taker", &filled, full, 990_000, "aa06").0, 200);
        let n = e.sw.store.all_matches().len();
        assert_eq!(take(&e, "tok-other", &filled, 100_000, 990_000, "aa07").0, 409);
        // cancelled
        let cancelled = ask(&e, 20_000_000, 990_000);
        let mut o = e.sw.store.offer(&cancelled).unwrap();
        o.status = OfferStatus::Cancelled;
        e.sw.store.put_offer(&o).unwrap();
        assert_eq!(take(&e, "tok-other", &cancelled, 100_000, 990_000, "aa08").0, 409);
        // expired
        let expired = ask(&e, 20_000_000, 990_000);
        let mut o = e.sw.store.offer(&expired).unwrap();
        o.expires_at = unix_now() - 1;
        e.sw.store.put_offer(&o).unwrap();
        assert_eq!(take(&e, "tok-other", &expired, 100_000, 990_000, "aa09").0, 409);
        // unknown
        assert_eq!(take(&e, "tok-other", "nope", 100_000, 990_000, "aa0a").0, 404);
        assert_eq!(e.sw.store.all_matches().len(), n, "no match was made");
    }

    #[test]
    fn own_private_tiny_and_oversized_takes_are_refused() {
        let e = edge();
        let chosen = ask(&e, 20_000_000, 1_000_000); // 200 000 sat BTC
        assert_eq!(take(&e, "tok-maker", &chosen, 100_000, 1_000_000, "aa0b").0, 409, "own offer");
        assert_eq!(take(&e, "tok-taker", &chosen, MIN_BTC_LEG_SAT - 1, 1_000_000, "aa0c").0, 400, "below the minimum");
        assert_eq!(take(&e, "tok-taker", &chosen, 200_001, 1_000_000, "aa0d").0, 400, "more than it holds");
        let (_, private) = e.priced("tok-maker", "xbt_to_btc", 20_000_000, 1_000_000, true);
        let pid = private["offer_id"].as_str().unwrap();
        let (s, r) = take(&e, "tok-taker", pid, 100_000, 1_000_000, "aa0e");
        assert_eq!(s, 409, "{r}");
        assert!(r["error"].as_str().unwrap().contains("code"));
        assert!(e.sw.store.all_matches().is_empty());
    }

    #[test]
    fn a_take_above_the_cap_is_refused() {
        let e = edge_with(new_swap_capped(Some(150_000)));
        let chosen = ask(&e, 15_000_000, 1_000_000);
        // Listed before the cap was lowered: it holds more than a take may move.
        let mut o = e.sw.store.offer(&chosen).unwrap();
        o.amount_sat = 20_000_000;
        e.sw.store.put_offer(&o).unwrap();
        let (s, r) = take(&e, "tok-taker", &chosen, 150_001, 1_000_000, "aa0f");
        assert_eq!(s, 400, "{r}");
        assert!(r["error"].as_str().unwrap().contains("maximum"));
        assert_eq!(take(&e, "tok-taker", &chosen, 150_000, 1_000_000, "aa10").0, 200);
    }

    #[test]
    fn a_struck_taker_cannot_take() {
        let e = edge();
        let chosen = ask(&e, 20_000_000, 1_000_000);
        for _ in 0..MAX_STRIKES {
            e.sw.store.add_strike("tok-taker", unix_now()).unwrap();
        }
        assert_eq!(take(&e, "tok-taker", &chosen, 100_000, 1_000_000, "aa11").0, 429);
    }

    #[test]
    fn a_take_needs_a_hex_client_id() {
        let e = edge();
        let chosen = ask(&e, 20_000_000, 1_000_000);
        assert_eq!(take(&e, "tok-taker", &chosen, 100_000, 1_000_000, "not hex").0, 400);
    }

    #[test]
    fn one_install_can_have_at_most_10_open_offers() {
        let e = edge();
        let mut ids = Vec::new();
        for i in 0..MAX_OPEN_OFFERS as u64 {
            let (s, o) = e.priced("tok-busy", "xbt_to_btc", 20_000_000, 2_000_000 + i, false);
            assert_eq!(s, 200, "{o}");
            ids.push((o["offer_id"].as_str().unwrap().to_string(), o["owner_secret"].as_str().unwrap().to_string()));
        }
        let (s, r) = e.priced("tok-busy", "xbt_to_btc", 20_000_000, 2_100_000, false);
        assert_eq!(s, 429, "{r}");
        assert!(r["error"].as_str().unwrap().contains("open offers"));
        // Someone else is unaffected.
        assert_eq!(e.priced("tok-other", "xbt_to_btc", 20_000_000, 2_100_000, false).0, 200);
        // Cancelling one makes room.
        let (id, secret) = &ids[0];
        assert_eq!(e.delete(&format!("/swap/offers/{id}"), "tok-busy", Some(secret)).0, 200);
        assert_eq!(e.priced("tok-busy", "xbt_to_btc", 20_000_000, 2_100_000, false).0, 200);
    }

    #[test]
    fn a_replay_is_answered_even_at_the_open_offer_limit() {
        let e = edge();
        let (s, first) = offer_with_client(&e, "tok-busy", Some("abcd"), false);
        assert_eq!(s, 200, "{first}");
        for i in 1..MAX_OPEN_OFFERS as u64 {
            assert_eq!(e.priced("tok-busy", "xbt_to_btc", 20_000_000, 2_000_000 + i, false).0, 200);
        }
        let (s, again) = offer_with_client(&e, "tok-busy", Some("abcd"), false);
        assert_eq!(s, 200, "{again}");
        assert_eq!(again["offer_id"], first["offer_id"]);
    }

    #[test]
    fn offer_requires_positive_price() {
        let e = edge();
        let missing = json!({ "direction": "btc_to_xbt", "amount_sat": 200_000 });
        let (s, r) = e.post("/swap/offers", "tok-a", None, missing);
        assert_eq!((s, r["error"].as_str()), (400, Some(PRICE_RANGE)));
        let (s, r) = e.priced("tok-a", "btc_to_xbt", 200_000, 0, false);
        assert_eq!((s, r["error"].as_str()), (400, Some(PRICE_RANGE)));
        assert_eq!(offers_of(&e, "tok-a"), 0);
    }

    const PRICE_RANGE: &str = "price_sat_per_xbt must be 1..=2100000000000000";

    #[test]
    fn offer_rejects_price_above_max() {
        let e = edge();
        let over = wallet_core::MAX_PRICE_SAT_PER_XBT + 1;
        for (dir, amount) in [("btc_to_xbt", 200_000), ("xbt_to_btc", 2)] {
            let (s, r) = e.priced("tok-a", dir, amount, over, false);
            assert_eq!((s, r["error"].as_str()), (400, Some(PRICE_RANGE)), "{dir}");
        }
        // The wrap case: 2 XBT at ~9.22e18 sat/XBT is far over the cap.
        let (s, r) = e.priced("tok-a", "xbt_to_btc", 200_000_000, 9_223_372_036_854_775_808, false);
        assert_eq!((s, r["error"].as_str()), (400, Some(PRICE_RANGE)));
        assert_eq!(offers_of(&e, "tok-a"), 0);
        // Exactly the cap is a valid price (a BTC leg is worth its amount).
        let (s, r) = e.priced("tok-a", "btc_to_xbt", 200_000, wallet_core::MAX_PRICE_SAT_PER_XBT, false);
        assert_eq!(s, 200, "{r}");
        // An XBT offer at the cap is judged on its leg, not refused for the price.
        let (s, r) = e.priced("tok-b", "xbt_to_btc", 1, wallet_core::MAX_PRICE_SAT_PER_XBT, false);
        assert_eq!(s, 400);
        assert_ne!(r["error"].as_str(), Some(PRICE_RANGE), "{r}");
    }

    #[test]
    fn offer_rejects_out_of_range_amounts() {
        let e = edge();
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 1_999, false).0, 400);
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 1_000_001, false).0, 400);
        // XBT amounts are judged by their BTC-leg value at the offer's own
        // price: at 1 008 980, 100 k XBT sat ≈ 1 008 BTC sat; 100 M ≈ 1 008 980
        // (over this test edge's 1 000 000 cap).
        assert_eq!(e.offer("tok-a", "xbt_to_btc", 100_000, false).0, 400);
        assert_eq!(e.offer("tok-a", "xbt_to_btc", 100_000_000, false).0, 400);
        let junk = json!({ "direction": "sideways", "amount_sat": 200_000, "price_sat_per_xbt": PRICE });
        assert_eq!(e.post("/swap/offers", "tok-a", None, junk).0, 400);

        let (s, o) = e.offer("tok-a", "btc_to_xbt", 20_000, false);
        assert_eq!(s, 200, "{o}");
        assert_eq!(o["owner_secret"].as_str().unwrap().len(), 64);
        assert!(o["share_code"].is_null());
        assert_eq!(o["price_sat_per_xbt"], PRICE);
        let (s, o) = e.offer("tok-a", "xbt_to_btc", 99_000_000, false);
        assert_eq!(s, 200, "{o}");
        // At 1 200 000, 2 M XBT sat is worth 24 000 BTC sat; 90 M, 1 080 000.
        assert_eq!(e.priced("tok-b", "xbt_to_btc", 2_000_000, 1_200_000, false).0, 200);
        assert_eq!(e.priced("tok-b", "xbt_to_btc", 90_000_000, 1_200_000, false).0, 400);
    }

    #[test]
    fn offer_worth_2_000_is_accepted() {
        let e = edge();
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 2_000, false).0, 200);
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 1_999, false).0, 400);
    }

    #[test]
    fn without_a_cap_there_is_no_maximum() {
        let (sw, db) = new_swap_capped(None);
        let e = edge_with((sw, db));
        let (s, q) = e.get("/swap/quote", "tok-a", None);
        assert_eq!(s, 200, "{q}");
        assert_eq!(q["max_btc_sat"], serde_json::Value::Null);
        // 50 BTC: far above the old 0.01 BTC default cap.
        assert_eq!(e.offer("tok-a", "btc_to_xbt", 5_000_000_000, false).0, 200);
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
        let (_, view) = e.get(&opath, "tok-c", o["owner_secret"].as_str());
        assert_eq!(view["status"], "open");
        assert_eq!(view["price_sat_per_xbt"], PRICE);
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
        let (s, p) = e.priced("tok-a", "btc_to_xbt", 200_000, 1_020_000, true);
        assert_eq!(s, 200, "{p}");
        let code = p["share_code"].as_str().unwrap().to_string();
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c)), "{code}");
        // A public opposite offer doesn't take a private one.
        assert!(e.offer("tok-b", "xbt_to_btc", 20_000_000, false).1["match_id"].is_null());

        assert_eq!(e.post("/swap/take/ZZZZZZZZ", "tok-c", None, json!({})).0, 404);
        assert_eq!(e.post(&format!("/swap/take/{code}"), "tok-a", None, json!({})).0, 409);
        // The body is ignored: the maker's price is the price.
        let (s, t) = e.post(
            &format!("/swap/take/{}", code.to_lowercase()),
            "tok-c",
            None,
            json!({ "max_slippage_bps": 75, "price_sat_per_xbt": 1 }),
        );
        assert_eq!(s, 200, "{t}");
        let id = t["match_id"].as_str().unwrap();
        let (s, m) = e.get(&format!("/swap/matches/{id}"), "tok-c", t["owner_secret"].as_str());
        assert_eq!(s, 200, "{m}");
        assert_eq!(m["role"], "initiator");
        assert_eq!(m["terms"]["price_sat_per_xbt"], 1_020_000);
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
    /// be able to cancel the match (no strike) and lock the XBT for 48 h.
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

    fn relayed(e: &Edge, id: &str, secret: &str) -> Vec<Value> {
        let (s, msgs) = e.get(&format!("/swap/matches/{id}/messages?after=0"), "tok", Some(secret));
        assert_eq!(s, 200, "{msgs}");
        msgs["messages"].as_array().unwrap().iter().map(|m| m["body"].clone()).collect()
    }

    /// A repeated accept is answered but not relayed again: otherwise one
    /// side could fill the relay (and the other's polls) with accepts.
    #[test]
    fn a_repeated_accept_is_relayed_once() {
        let e = edge();
        let (id, init, part) = e.matched();
        assert!(e.msg(&id, &init, json!({"type": "accept"})).1["seq"].is_u64());
        let (s, r) = e.msg(&id, &init, json!({"type": "accept"}));
        assert_eq!((s, &r["seq"]), (200, &Value::Null), "{r}");
        assert!(e.msg(&id, &part, json!({"type": "accept"})).1["seq"].is_u64());
        for who in [&init, &part] {
            let (s, r) = e.msg(&id, who, json!({"type": "accept"}));
            assert_eq!((s, &r["seq"]), (200, &Value::Null), "accepted already: {r}");
        }
        assert_eq!(relayed(&e, &id, &part).len(), 2);
    }

    /// The other side gets the message as the edge understood it, nothing more.
    #[test]
    fn relayed_messages_are_canonical() {
        let e = edge();
        let (id, init, part) = e.matched();
        let upper = PUBKEY.to_uppercase();
        let junk = "j".repeat(500);
        assert_eq!(e.msg(&id, &init, json!({"type": "accept", "note": junk})).0, 200);
        assert_eq!(e.msg(&id, &init, json!({"type": "keys", "xbt": upper, "btc": PUBKEY, "note": junk})).0, 200);
        assert_eq!(e.msg(&id, &init, json!({"type": "hashlock", "hex": "AB".repeat(32), "note": junk})).0, 200);
        assert_eq!(
            relayed(&e, &id, &part),
            [
                json!({"type": "accept"}),
                json!({"type": "keys", "xbt": PUBKEY, "btc": PUBKEY}),
                json!({"type": "hashlock", "hex": "ab".repeat(32)}),
            ]
        );
    }

    // ---- the cooperative release (v2) ----

    /// New matches use the v2 contract (with the release branch).
    #[test]
    fn new_matches_use_the_v2_contract() {
        let e = edge();
        let (id, _, _) = e.matched();
        assert_eq!(e.sw.store.get_match(&id).unwrap().terms.contract_version, wallet_core::htlc::CURRENT_CONTRACT);
    }

    /// An unsigned single-input spend of `op`, as a release request carries.
    fn release_tx_hex(op: OutPoint) -> String {
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn { previous_output: op, script_sig: ScriptBuf::new(), sequence: Sequence::ENABLE_RBF_NO_LOCKTIME, witness: Witness::new() }],
            output: vec![TxOut { value: Amount::from_sat(9_000), script_pubkey: ScriptBuf::from(vec![0u8; 22]) }],
        };
        bitcoin::consensus::encode::serialize_hex(&tx)
    }

    /// A match cancelled with the initiator's XBT locked and nothing of the
    /// participant's: (match id, initiator secret, participant secret, the XBT outpoint).
    fn cancelled_with_xbt_locked(e: &Edge) -> (String, String, String, OutPoint) {
        let (id, init, part) = e.matched();
        let op = OutPoint { txid: txid(0xaa), vout: 1 };
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.xbt_funding = Some((op.txid.to_string(), op.vout));
        m.status = MatchStatus::Cancelled;
        e.sw.store.put_match(&m).unwrap();
        (id, init, part, op)
    }

    /// The initiator asks for its XBT back; the participant answers with its
    /// signatures; both are relayed as the edge understood them, once.
    #[test]
    fn a_cancelled_match_relays_the_release() {
        let e = edge();
        let (id, init, part, op) = cancelled_with_xbt_locked(&e);
        let txs = vec![release_tx_hex(op).to_uppercase(), release_tx_hex(op)];
        let (s, r) = e.msg(&id, &init, json!({"type": "release_request", "txs": txs}));
        assert_eq!(s, 200, "{r}");
        let sigs = vec!["30".repeat(35), "30".repeat(36)];
        let (s, r) = e.msg(&id, &part, json!({"type": "release", "sigs": sigs}));
        assert_eq!(s, 200, "{r}");
        assert_eq!(
            relayed(&e, &id, &part),
            [
                json!({"type": "release_request", "txs": [release_tx_hex(op), release_tx_hex(op)]}),
                json!({"type": "release", "sigs": sigs}),
            ]
        );
        // set once: the same again is answered, different refused
        assert_eq!(e.msg(&id, &part, json!({"type": "release", "sigs": sigs})).1["seq"], Value::Null);
        assert_eq!(e.msg(&id, &part, json!({"type": "release", "sigs": ["31".repeat(35), "30".repeat(36)]})).0, 409);
        // released: the participant can't fund this match any more
        let (s, r) = e.msg(&id, &part, funded_msg("btc", OutPoint { txid: txid(0xbb), vout: 0 }));
        assert_eq!(s, 409, "{r}");
    }

    #[test]
    fn a_release_is_only_for_a_cancelled_match_with_only_the_initiators_coins_locked() {
        let e = edge();
        let (id, init, part) = e.matched();
        let op = OutPoint { txid: txid(0xaa), vout: 1 };
        let ask = json!({"type": "release_request", "txs": [release_tx_hex(op)]});
        assert_eq!(e.msg(&id, &init, ask.clone()).0, 409, "the match is live");

        let (id, init, part2, op) = cancelled_with_xbt_locked(&e);
        let _ = part;
        let ask = |txs: Vec<String>| json!({"type": "release_request", "txs": txs});
        assert_eq!(e.msg(&id, &part2, ask(vec![release_tx_hex(op)])).0, 403, "only the initiator asks");
        assert_eq!(e.msg(&id, &init, ask(vec![release_tx_hex(OutPoint { txid: txid(0xcc), vout: 1 })])).0, 400, "another coin");
        assert_eq!(e.msg(&id, &init, ask(vec![])).0, 400, "none");
        assert_eq!(e.msg(&id, &init, ask(vec![release_tx_hex(op); 5])).0, 400, "at most 4");
        assert_eq!(e.msg(&id, &init, ask(vec!["zz".into()])).0, 400, "not a transaction");
        assert_eq!(e.msg(&id, &init, json!({"type": "release", "sigs": ["30".repeat(35)]})).0, 403, "only the participant signs");
        assert_eq!(e.msg(&id, &part2, json!({"type": "release", "sigs": ["30".repeat(35)]})).0, 409, "nothing asked yet");
        assert_eq!(e.msg(&id, &init, ask(vec![release_tx_hex(op)])).0, 200);
        assert_eq!(e.msg(&id, &part2, json!({"type": "release", "sigs": ["30".repeat(35), "30".repeat(35)]})).0, 400, "one per tx");
        assert_eq!(e.msg(&id, &part2, json!({"type": "release", "sigs": ["30".repeat(80)]})).0, 400, "not a signature");

        // the participant's BTC is locked: no release (its coins are in the swap)
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.btc_funding = Some((txid(0xbb).to_string(), 0));
        e.sw.store.put_match(&m).unwrap();
        assert_eq!(e.msg(&id, &part2, json!({"type": "release", "sigs": ["30".repeat(35)]})).0, 409);
    }

    /// The initiator's preimage nonce rides with its hashlock (a restored phone
    /// reads it back to rebuild its preimage): relayed as 16 bytes of lowercase
    /// hex, anything else refused.
    #[test]
    fn the_hashlock_carries_the_preimage_nonce() {
        let e = edge();
        let (id, init, part) = e.matched();
        let msg = |n: Value| json!({"type": "hashlock", "hex": "ab".repeat(32), "nonce": n});
        assert_eq!(e.msg(&id, &init, msg(json!("0a".repeat(15)))).0, 400);
        assert_eq!(e.msg(&id, &init, msg(json!("zz".repeat(16)))).0, 400);
        assert_eq!(e.msg(&id, &init, msg(json!(7))).0, 400);
        assert_eq!(e.msg(&id, &init, msg(json!("0A".repeat(16)))).0, 200);
        assert_eq!(
            relayed(&e, &id, &part),
            [json!({"type": "hashlock", "hex": "ab".repeat(32), "nonce": "0a".repeat(16)})]
        );
        // the initiator reads its own back, as a restored phone does
        assert_eq!(relayed(&e, &id, &init).len(), 1);
    }

    #[test]
    fn a_message_over_4_kib_is_refused() {
        let e = edge();
        let (id, init, _) = e.matched();
        let (s, r) = e.msg(&id, &init, json!({"type": "accept", "note": "j".repeat(MAX_MESSAGE_BODY as usize)}));
        assert_eq!(s, 413, "{r}");
        // (The store, not a poll: an empty relay long-polls.)
        assert!(e.sw.store.msgs_after(&id, 0).is_empty());
        assert_eq!(e.sw.store.get_match(&id).unwrap().accepted, (false, false));
    }

    /// A backstop: whatever the message types allow, one match's relay holds
    /// at most MAX_MESSAGES_PER_MATCH messages.
    #[test]
    fn a_match_relays_at_most_its_message_cap() {
        let e = edge();
        let (id, init, part) = e.matched();
        for _ in 0..MAX_MESSAGES_PER_MATCH {
            relay::push(&e.sw.store, &id, "edge", json!({"type": "deadline"})).unwrap();
        }
        let (s, r) = e.msg(&id, &init, json!({"type": "accept"}));
        assert_eq!(s, 429, "{r}");
        assert_eq!(relayed(&e, &id, &part).len(), MAX_MESSAGES_PER_MATCH);
        assert_eq!(e.sw.store.get_match(&id).unwrap().accepted, (false, false), "refused before it changed anything");
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
                               "price_sat_per_xbt": PRICE, "private": private });
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
                price_sat_per_xbt: PRICE,
                created_at: now,
                expires_at: now + DEFAULT_EXPIRES_SECS,
                private: false,
                share_code: None,
                status: OfferStatus::Open,
                match_id: None,
                client_id: None,
                recovery_tag: None,
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

    /// Both sides accept and relay keys (`party(1)`, `party(3)`) and the
    /// hashlock; the initiator's XBT HTLC is on chain at `txid(0xaa):1` and
    /// the initiator posts it as its funding.
    fn funded(e: &Edge) -> (String, String, String, OutPoint) {
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(e, &id, &init, &part);
        let t = terms_of(e, &id, &init);
        let htlc = OutPoint { txid: txid(0xaa), vout: 1 };
        e.chain.put(Chain::Xbt, htlc, t.xbt_sat, &xbt_leg(&t, &i, &p).script_pubkey);
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
        HtlcContract::build(&xbt, wallet_core::htlc::CURRENT_CONTRACT, hashlock(), &redeem, &refund, t.xbt_locktime, Amount::from_sat(t.xbt_sat))
    }

    /// The participant's BTC HTLC: the initiator redeems, the participant refunds.
    fn btc_leg(t: &SwapTerms, init: &Party, part: &Party) -> HtlcContract {
        let (btc, _) = network_params(&t.network).unwrap();
        let (redeem, refund) = (init.btc.pubkey(), part.btc.pubkey());
        HtlcContract::build(&btc, wallet_core::htlc::CURRENT_CONTRACT, hashlock(), &redeem, &refund, t.btc_locktime, Amount::from_sat(t.btc_sat))
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
        // Without both keys (a funding recorded before they were relayed —
        // `funded` no longer allows that): no script to check against.
        let with_keys = e.sw.store.get_match(&id).unwrap();
        let mut keyless = with_keys.clone();
        keyless.keys.1 = None;
        e.sw.store.put_match(&keyless).unwrap();
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&real)));
        e.sw.store.put_match(&with_keys).unwrap();

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
        // Every HTLC spend has exactly one input: two — even the real one
        // twice — is not one (and can't make the check scan per input).
        let mut doubled = real.clone();
        doubled.input.push(real.input[0].clone());
        assert!(!is_swap_spend(&e.sw, Chain::Xbt, &tx_hex(&doubled)));
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
        let t = terms_of(&e, &id, &init);
        e.chain.put(Chain::Btc, btc, t.btc_sat, &btc_leg(&t, &i, &p).script_pubkey);
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
        /// Block times of mined transactions; absent = unconfirmed.
        mined: Mutex<std::collections::HashMap<(Chain, String), u64>>,
        /// `tx_block_time` fails (the node unreachable).
        block_time_down: std::sync::atomic::AtomicBool,
        /// `txout` and `created_output` fail (the node unreachable).
        txout_down: std::sync::atomic::AtomicBool,
        /// Every output `put`, spent or not.
        created: Mutex<std::collections::HashMap<(Chain, String, u32), watchtower::TxOutInfo>>,
    }

    impl StubChain {
        fn mine(&self, chain: Chain, txid: Txid, block_time: u64) {
            self.mined.lock().unwrap().insert((chain, txid.to_string()), block_time);
        }

        fn put(&self, chain: Chain, at: OutPoint, value_sat: u64, spk: &ScriptBuf) {
            let out = watchtower::TxOutInfo {
                value_sat,
                scriptpubkey_hex: hex::encode(spk.as_bytes()),
                confirmations: 0,
            };
            let key = (chain, at.txid.to_string(), at.vout);
            self.txouts.lock().unwrap().insert(key.clone(), out.clone());
            self.created.lock().unwrap().insert(key, out);
        }

        /// `at` is spent: gone from the UTXO view, still in its transaction.
        fn spend(&self, chain: Chain, at: OutPoint) {
            self.txouts.lock().unwrap().remove(&(chain, at.txid.to_string(), at.vout));
        }
    }

    impl watchtower::ChainReader for StubChain {
        fn txout(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<watchtower::TxOutInfo>> {
            if self.txout_down.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("node unreachable");
            }
            Ok(self.txouts.lock().unwrap().get(&(chain, txid.to_string(), vout)).cloned())
        }
        fn created_output(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<watchtower::TxOutInfo>> {
            if self.txout_down.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("node unreachable");
            }
            Ok(self.created.lock().unwrap().get(&(chain, txid.to_string(), vout)).cloned())
        }
        fn spending_tx_hex(&self, _: Chain, _: &str, _: u32) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn spend_confirmed(&self, _: Chain, _: &str, _: u32) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn tx_block_time(&self, chain: Chain, txid: &str) -> anyhow::Result<Option<u64>> {
            if self.block_time_down.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("node unreachable");
            }
            Ok(self.mined.lock().unwrap().get(&(chain, txid.to_string())).copied())
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
        let e = edge();
        let chain = Arc::clone(&e.chain);
        (e, chain)
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

    fn funded_msg(chain: &str, at: OutPoint) -> Value {
        json!({"type": "funded", "chain": chain, "txid": at.txid.to_string(), "vout": at.vout})
    }

    /// Both accepted, keys and hashlock relayed: the initiator's turn to fund.
    /// Returns (id, initiator secret, terms, the XBT leg).
    fn ready_to_fund(e: &Edge) -> (String, String, SwapTerms, HtlcContract) {
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(e, &id, &init, &part);
        let t = terms_of(e, &id, &init);
        let leg = xbt_leg(&t, &i, &p);
        (id, init, t, leg)
    }

    fn assert_not_funded(e: &Edge, id: &str) {
        let m = e.sw.store.get_match(id).unwrap();
        assert_eq!(m.status, MatchStatus::Accepted);
        assert_eq!(m.xbt_funding, None);
    }

    /// `funded` only names an outpoint: the match moves on only when the
    /// chain shows that outpoint pays exactly this leg's HTLC script and
    /// amount. Otherwise a party could claim a funding it never made — to
    /// dodge its deadline and strike, or to lure the other side into funding.
    #[test]
    fn funded_must_be_this_legs_htlc_on_chain() {
        let e = edge();
        let (id, init, t, leg) = ready_to_fund(&e);

        // Not seen on chain (yet): refused, and the phone retries.
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", OutPoint { txid: txid(0xa1), vout: 0 }));
        assert_eq!(s, 503, "{r}");
        // The right script on the other chain isn't XBT funding.
        let btc_only = OutPoint { txid: txid(0xa2), vout: 0 };
        e.chain.put(Chain::Btc, btc_only, t.xbt_sat, &leg.script_pubkey);
        assert_eq!(e.msg(&id, &init, funded_msg("xbt", btc_only)).0, 503);
        // One sat short.
        let short = OutPoint { txid: txid(0xa3), vout: 0 };
        e.chain.put(Chain::Xbt, short, t.xbt_sat - 1, &leg.script_pubkey);
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", short));
        assert_eq!(s, 409, "{r}");
        // More than the terms is wrong too.
        let over = OutPoint { txid: txid(0xa4), vout: 0 };
        e.chain.put(Chain::Xbt, over, t.xbt_sat + 1, &leg.script_pubkey);
        assert_eq!(e.msg(&id, &init, funded_msg("xbt", over)).0, 409);
        // Another HTLC (other keys) with the right amount.
        let other = OutPoint { txid: txid(0xa5), vout: 0 };
        e.chain.put(Chain::Xbt, other, t.xbt_sat, &xbt_leg(&t, &party(5), &party(7)).script_pubkey);
        assert_eq!(e.msg(&id, &init, funded_msg("xbt", other)).0, 409);
        assert_not_funded(&e, &id);

        let real = OutPoint { txid: txid(0xaa), vout: 1 };
        e.chain.put(Chain::Xbt, real, t.xbt_sat, &leg.script_pubkey);
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", real));
        assert_eq!(s, 200, "{r}");
        let m = e.sw.store.get_match(&id).unwrap();
        assert_eq!(m.status, MatchStatus::InitiatorFunded);
        assert_eq!(m.xbt_funding, Some((real.txid.to_string(), 1)));
    }

    /// A late `funded` (say the phone couldn't reach the edge) for an HTLC
    /// already redeemed: the funding happened, so it still counts.
    #[test]
    fn funded_for_an_htlc_already_spent_still_counts() {
        let e = edge();
        let (id, init, t, leg) = ready_to_fund(&e);
        let real = OutPoint { txid: txid(0xaa), vout: 1 };
        e.chain.put(Chain::Xbt, real, t.xbt_sat, &leg.script_pubkey);
        e.chain.spend(Chain::Xbt, real);
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", real));
        assert_eq!(s, 200, "{r}");
        assert_eq!(e.sw.store.get_match(&id).unwrap().status, MatchStatus::InitiatorFunded);
    }

    #[test]
    fn funded_before_keys_and_hashlock_is_refused() {
        let e = edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", OutPoint { txid: txid(0xaa), vout: 1 }));
        assert_eq!(s, 409, "{r}");
        assert!(r["error"].as_str().unwrap().contains("keys and the hashlock"), "{r}");
        assert_not_funded(&e, &id);
    }

    #[test]
    fn funded_while_the_chain_cannot_be_read_is_retryable() {
        let e = edge();
        let (id, init, t, leg) = ready_to_fund(&e);
        let real = OutPoint { txid: txid(0xaa), vout: 1 };
        e.chain.put(Chain::Xbt, real, t.xbt_sat, &leg.script_pubkey);
        e.chain.txout_down.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, r) = e.msg(&id, &init, funded_msg("xbt", real));
        assert_eq!(s, 502, "{r}");
        // The reader's own error (it can name internal addresses) goes to the log, not the reply.
        assert_eq!(r["error"], "the chain is unavailable");
        assert_not_funded(&e, &id);
        e.chain.txout_down.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(e.msg(&id, &init, funded_msg("xbt", real)).0, 200);
    }

    /// A participant's BTC funding is checked the same way.
    #[test]
    fn participant_funded_must_be_the_btc_htlc_on_chain() {
        let e = edge();
        let (id, init, part, _) = funded(&e);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let fake = OutPoint { txid: txid(0xbb), vout: 0 };
        assert_eq!(e.msg(&id, &part, funded_msg("btc", fake)).0, 503);
        e.chain.put(Chain::Btc, fake, t.btc_sat, &xbt_leg(&t, &i, &p).script_pubkey);
        assert_eq!(e.msg(&id, &part, funded_msg("btc", fake)).0, 409, "the XBT leg's script is not the BTC leg's");
        assert_eq!(e.sw.store.get_match(&id).unwrap().status, MatchStatus::InitiatorFunded);
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

    // ---- phone logs: `POST /swap/offers/{id}/log` ----

    #[test]
    fn log_lines_are_validated_trimmed_and_cleaned() {
        assert_eq!(log_lines(&json!({"lines": ["a", "b"]})).ok(), Some(vec!["a".to_string(), "b".to_string()]));
        // control characters can't forge journal lines; long lines are cut
        let long = "x".repeat(LOG_MAX_CHARS + 50);
        let got = log_lines(&json!({"lines": ["evil\nfortis-edge: fake\u{1b}[31m", long]})).ok().unwrap();
        assert_eq!(got[0], "evil fortis-edge: fake [31m");
        assert_eq!(got[1].chars().count(), LOG_MAX_CHARS);
        // Nor invisible Unicode (a right-to-left override, a zero-width space):
        // anything but printable ASCII is written as a visible escape.
        let got = log_lines(&json!({"lines": ["paid \u{202e}0001\u{200b} é"]})).ok().unwrap();
        assert_eq!(got[0], "paid \\u{202e}0001\\u{200b} \\u{e9}");
        assert!(log_lines(&json!({"lines": "a"})).is_err());
        assert!(log_lines(&json!({"lines": [1]})).is_err());
        assert!(log_lines(&json!({})).is_err());
        let too_many: Vec<String> = (0..=LOG_MAX_LINES).map(|i| i.to_string()).collect();
        assert!(log_lines(&json!({ "lines": too_many })).is_err());
    }

    #[test]
    fn the_log_quota_is_per_offer_per_minute() {
        let q = LogQuota::default();
        assert!(q.take("o1", LOG_LINES_PER_MIN as usize, 60));
        assert!(!q.take("o1", 1, 61), "spent for this minute");
        assert!(q.take("o2", 1, 61), "another offer has its own");
        assert!(q.take("o1", 1, 120), "a new minute");
    }

    #[test]
    fn only_the_offers_owner_can_log_to_it() {
        let e = edge();
        let (id, init, part) = e.matched();
        let m = e.sw.store.get_match(&id).unwrap();
        let body = json!({ "lines": ["17080b90: step FUNDED role=initiator"] });
        let path = format!("/swap/offers/{}/log", m.initiator_offer);
        assert_eq!(e.post(&path, "tok", Some(&init), body.clone()).0, 200);
        assert_eq!(e.post(&path, "tok", Some(&part), body.clone()).0, 403, "the other side's secret");
        assert_eq!(e.post(&path, "tok", None, body.clone()).0, 403);
        assert_eq!(e.post("/swap/offers/nope/log", "tok", Some(&init), body).0, 404);
        assert_eq!(e.post(&path, "tok", Some(&init), json!({ "lines": "x" })).0, 400);
    }

    // ---- a participant who funded in time, but whose `funded` arrived late ----

    /// Initiator funded; the participant's clock ran out before its `funded`
    /// arrived, so the deadline tick cancelled the match and struck the
    /// participant. The participant's BTC HTLC `htlc` is on chain (not mined
    /// yet: callers mine it at the time they need). Returns
    /// (edge, chain, id, participant secret, participant party, btc leg, htlc, fund_by).
    fn missed_deadline() -> (Edge, Arc<StubChain>, String, String, Party, HtlcContract, OutPoint, u64) {
        let (e, chain) = chain_edge();
        let (id, init, part) = e.matched();
        assert_eq!(e.msg(&id, &init, json!({"type": "accept"})).0, 200);
        assert_eq!(e.msg(&id, &part, json!({"type": "accept"})).0, 200);
        let (i, p) = exchange_keys(&e, &id, &init, &part);
        let t = terms_of(&e, &id, &init);
        let xbt = OutPoint { txid: txid(0xaa), vout: 1 };
        chain.put(Chain::Xbt, xbt, t.xbt_sat, &xbt_leg(&t, &i, &p).script_pubkey);
        let funded = json!({"type": "funded", "chain": "xbt", "txid": xbt.txid.to_string(), "vout": 1});
        assert_eq!(e.msg(&id, &init, funded).0, 200);
        let fund_by = unix_now() - 3600;
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.participant_fund_by = Some(fund_by as u32);
        e.sw.store.put_match(&m).unwrap();
        matcher::tick(&e.sw.store, fund_by + 1);
        let m = e.sw.store.get_match(&id).unwrap();
        assert_eq!(m.status, MatchStatus::Cancelled);
        assert!(m.missed_participant_deadline);
        assert_eq!(e.sw.store.strikes(&m.participant_token, unix_now()), 1);
        let leg = btc_leg(&t, &i, &p);
        let htlc = OutPoint { txid: txid(0xbb), vout: 0 };
        chain.put(Chain::Btc, htlc, t.btc_sat, &leg.script_pubkey);
        (e, chain, id, part, p, leg, htlc, fund_by)
    }

    fn btc_funded(htlc: OutPoint) -> Value {
        json!({"type": "funded", "chain": "btc", "txid": htlc.txid.to_string(), "vout": htlc.vout})
    }

    fn assert_reinstated(e: &Edge, id: &str, htlc: OutPoint) {
        let m = e.sw.store.get_match(id).unwrap();
        assert_eq!(m.status, MatchStatus::ParticipantFunded);
        assert!(!m.missed_participant_deadline);
        assert_eq!(m.closed_at, None);
        assert_eq!(m.btc_funding, Some((htlc.txid.to_string(), htlc.vout)));
        assert_eq!(e.sw.store.strikes(&m.participant_token, unix_now()), 0, "the strike is lifted");
        for offer in [&m.initiator_offer, &m.participant_offer] {
            let o = e.sw.store.offer(offer).unwrap();
            assert_eq!(o.status, OfferStatus::Cancelled, "offer {offer} stays off the book");
        }
    }

    fn assert_still_cancelled(e: &Edge, id: &str) {
        let m = e.sw.store.get_match(id).unwrap();
        assert_eq!(m.status, MatchStatus::Cancelled);
        assert_eq!(e.sw.store.strikes(&m.participant_token, unix_now()), 1);
    }

    #[test]
    fn a_late_funded_for_btc_mined_before_the_deadline_reinstates_the_match() {
        let (e, chain, id, part, _, _, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by - 600);
        let (s, r) = e.msg(&id, &part, btc_funded(htlc));
        assert_eq!(s, 200, "{r}");
        assert!(r["seq"].is_u64(), "relayed, so the initiator hears of it: {r}");
        assert_reinstated(&e, &id, htlc);
        // A repeat is a no-op.
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)), (200, json!({ "seq": Value::Null })));
    }

    #[test]
    fn the_participants_refund_watch_of_btc_mined_in_time_reinstates_the_match() {
        let (e, chain, id, part, p, leg, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by);
        let refund = signed_refund(&leg, htlc, &p.btc);
        let (s, r) = refund_watch_on(&e, &id, &part, "btc", htlc, &refund);
        assert_eq!(s, 200, "{r}");
        assert_reinstated(&e, &id, htlc);
        // `funded` afterwards is a plain no-op.
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)).0, 200);
        assert_reinstated(&e, &id, htlc);
    }

    /// A match whose participant released the initiator's XBT is never
    /// reinstated: the initiator may have its coins back already.
    #[test]
    fn a_released_match_is_never_reinstated() {
        let (e, chain, id, part, p, leg, htlc, fund_by) = missed_deadline();
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.release_sigs = Some(vec!["30".repeat(35)]);
        e.sw.store.put_match(&m).unwrap();
        chain.mine(Chain::Btc, htlc.txid, fund_by);
        let refund = signed_refund(&leg, htlc, &p.btc);
        let _ = refund_watch_on(&e, &id, &part, "btc", htlc, &refund);
        assert_still_cancelled(&e, &id);
    }

    /// A v1 match has no release branch: nothing to relay.
    #[test]
    fn a_v1_match_relays_no_release() {
        let e = edge();
        let (id, init, _part, op) = cancelled_with_xbt_locked(&e);
        let mut m = e.sw.store.get_match(&id).unwrap();
        m.terms.contract_version = wallet_core::htlc::CONTRACT_V1;
        e.sw.store.put_match(&m).unwrap();
        assert_eq!(e.msg(&id, &init, json!({"type": "release_request", "txs": [release_tx_hex(op)]})).0, 409);
    }

    #[test]
    fn btc_mined_after_the_deadline_leaves_the_match_cancelled_and_says_so_finally() {
        let (e, chain, id, part, p, leg, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by + 1);
        // 410, not 409: final — the phone stops re-announcing it.
        let (s, r) = e.msg(&id, &part, btc_funded(htlc));
        assert_eq!(s, 410, "{r}");
        assert!(r["error"].as_str().unwrap().contains("after its deadline"), "{r}");
        assert_still_cancelled(&e, &id);
        // Its refund is still watched: the participant's coins must come back.
        let refund = signed_refund(&leg, htlc, &p.btc);
        assert_eq!(refund_watch_on(&e, &id, &part, "btc", htlc, &refund).0, 200);
        assert_still_cancelled(&e, &id);
    }

    #[test]
    fn unmined_btc_funding_does_not_reinstate_yet_but_a_later_try_does() {
        let (e, chain, id, part, _, _, htlc, fund_by) = missed_deadline();
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)).0, 409);
        assert_still_cancelled(&e, &id);
        chain.mine(Chain::Btc, htlc.txid, fund_by - 1);
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)).0, 200);
        assert_reinstated(&e, &id, htlc);
    }

    #[test]
    fn a_late_funded_needs_the_exact_btc_htlc_output() {
        let (e, chain, id, part, _, leg, _, fund_by) = missed_deadline();
        let short = OutPoint { txid: txid(0xcc), vout: 0 };
        chain.put(Chain::Btc, short, 1_000, &leg.script_pubkey);
        chain.mine(Chain::Btc, short.txid, fund_by - 1);
        assert_eq!(e.msg(&id, &part, btc_funded(short)).0, 409);
        let unknown = OutPoint { txid: txid(0xdd), vout: 0 };
        assert_eq!(e.msg(&id, &part, btc_funded(unknown)).0, 409);
        assert_still_cancelled(&e, &id);
        assert_eq!(e.sw.store.get_match(&id).unwrap().btc_funding, None);
    }

    #[test]
    fn a_match_cancelled_for_another_reason_is_never_reinstated() {
        let htlc = OutPoint { txid: txid(0xbb), vout: 0 };
        // Cancelled at acceptance: nothing was ever funded.
        let (e2, _) = chain_edge();
        let (id2, _, part2) = e2.matched();
        let t2 = terms_of(&e2, &id2, &part2);
        matcher::tick(&e2.sw.store, t2.accept_by as u64 + 1);
        assert_eq!(e2.msg(&id2, &part2, btc_funded(htlc)).0, 409);
        assert_eq!(e2.sw.store.get_match(&id2).unwrap().status, MatchStatus::Cancelled);
        assert!(!e2.sw.store.get_match(&id2).unwrap().missed_participant_deadline);
    }

    #[test]
    fn a_late_funded_racing_a_reinstatement_by_the_refund_watch_is_a_no_op() {
        let (e, chain, id, part, p, leg, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by - 1);
        // The phone's `funded` read the match while it was still cancelled…
        let stale = e.sw.store.get_match(&id).unwrap();
        // …then its refund watch reinstated it first.
        let refund = signed_refund(&leg, htlc, &p.btc);
        assert_eq!(refund_watch_on(&e, &id, &part, "btc", htlc, &refund).0, 200);
        assert_reinstated(&e, &id, htlc);
        match late_participant_funded(&e.sw, &stale, htlc, || Reply::Json(200, json!({ "seq": 1 }))) {
            Reply::Json(200, _) => {}
            Reply::Json(code, v) => panic!("expected a no-op 200, got {code} {v}"),
            _ => panic!("expected a JSON reply"),
        }
        assert_reinstated(&e, &id, htlc);
    }

    #[test]
    fn a_reinstated_match_is_not_cancelled_again_by_the_next_deadline_tick() {
        let (e, chain, id, part, _, _, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by - 1);
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)).0, 200);
        matcher::tick(&e.sw.store, unix_now() + 7 * 24 * 3600);
        assert_reinstated(&e, &id, htlc);
    }

    #[test]
    fn the_refund_watch_is_stored_even_when_the_reinstatement_check_cant_read_the_chain() {
        let (e, chain, id, part, p, leg, htlc, fund_by) = missed_deadline();
        chain.mine(Chain::Btc, htlc.txid, fund_by - 1);
        chain.block_time_down.store(true, std::sync::atomic::Ordering::SeqCst);
        let refund = signed_refund(&leg, htlc, &p.btc);
        let (s, r) = refund_watch_on(&e, &id, &part, "btc", htlc, &refund);
        assert_eq!(s, 200, "{r}");
        assert_eq!(e.sw.store.watch_jobs().len(), 1, "the refund is with the watchtower");
        assert_still_cancelled(&e, &id);
        // Once the chain answers, a later `funded` reinstates it.
        chain.block_time_down.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(e.msg(&id, &part, btc_funded(htlc)).0, 200);
        assert_reinstated(&e, &id, htlc);
    }

    fn refund_watch_on(e: &Edge, id: &str, secret: &str, chain: &str, at: OutPoint, refund: &Transaction) -> (u16, Value) {
        e.post(
            "/swap/watch",
            "tok",
            Some(secret),
            json!({ "match_id": id, "chain": chain, "kind": "refund",
                    "htlc_outpoint": { "txid": at.txid.to_string(), "vout": at.vout },
                    "not_before": 1_900_000_000u32, "variants": [tx_hex(refund)] }),
        )
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
