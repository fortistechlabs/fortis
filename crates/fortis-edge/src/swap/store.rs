//! Durable state for P2P swaps: offers, matches, the per-match message relay,
//! watchtower jobs and anti-griefing strikes. Every record is a serde JSON
//! value in its own RocksDB column family.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Context};
use rocksdb::{ColumnFamily, Direction as IterDirection, IteratorMode, Options, DB};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wallet_core::swap_terms::SwapTerms;
use wallet_core::Chain;

const CF_OFFERS: &str = "offers";
const CF_MATCHES: &str = "matches";
const CF_MSGS: &str = "msgs";
const CF_WATCH: &str = "watch";
const CF_STRIKES: &str = "strikes";
const ALL_CFS: [&str; 5] = [CF_OFFERS, CF_MATCHES, CF_MSGS, CF_WATCH, CF_STRIKES];

pub const STRIKE_WINDOW_SECS: u64 = 24 * 3600;
pub const PURGE_AFTER_SECS: u64 = 7 * 24 * 3600;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Sends XBT, receives BTC: the swap initiator.
    XbtToBtc,
    /// Sends BTC, receives XBT: the swap participant.
    BtcToXbt,
}

impl Direction {
    pub fn opposite(self) -> Self {
        match self {
            Direction::XbtToBtc => Direction::BtcToXbt,
            Direction::BtcToXbt => Direction::XbtToBtc,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OfferStatus {
    Open,
    Matched,
    Cancelled,
    Expired,
    Filled,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Offer {
    pub id: String,
    pub token: String,
    pub secret_hash: [u8; 32],
    pub direction: Direction,
    /// On the user's own send chain (XBT sats for `XbtToBtc`, BTC sats for `BtcToXbt`).
    pub amount_sat: u64,
    pub max_slippage_bps: u32,
    pub created_at: u64,
    pub expires_at: u64,
    pub private: bool,
    pub share_code: Option<String>,
    pub status: OfferStatus,
    pub match_id: Option<String>,
    /// The client's idempotency key (lowercase hex). A repeat `POST
    /// /swap/offers` with the same token and key returns this offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Proposed,
    Accepted,
    InitiatorFunded,
    ParticipantFunded,
    Closed,
    Cancelled,
}

/// A party's relayed swap pubkeys, compressed hex: `(xbt, btc)`.
pub type KeysHex = (String, String);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Match {
    pub id: String,
    pub terms: SwapTerms,
    pub initiator_offer: String,
    pub participant_offer: String,
    pub initiator_token: String,
    pub participant_token: String,
    pub initiator_secret_hash: [u8; 32],
    pub participant_secret_hash: [u8; 32],
    /// (initiator, participant)
    pub accepted: (bool, bool),
    pub status: MatchStatus,
    pub xbt_funding: Option<(String, u32)>,
    pub btc_funding: Option<(String, u32)>,
    pub participant_fund_by: Option<u32>,
    /// (initiator, participant) offer amounts before the fill.
    pub pre_match_amounts: (u64, u64),
    pub closed_at: Option<u64>,
    /// Relayed swap pubkeys as (initiator, participant). Each is fixed once set.
    #[serde(default)]
    pub keys: (Option<KeysHex>, Option<KeysHex>),
    /// The initiator's relayed hashlock, hex. Fixed once set.
    #[serde(default)]
    pub hashlock: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    Refund,
    Redeem,
    /// A preimage-less redeem: `variants[i]` is finished with `sigs_hex[i]`
    /// (each feerate variant has its own signature) and the preimage.
    WaitingRedeem { pubkey_hex: String, witness_script_hex: String, sigs_hex: Vec<String> },
}

impl WatchKind {
    pub(crate) fn key_tag(&self) -> &'static str {
        match self {
            WatchKind::Refund => "refund",
            WatchKind::Redeem => "redeem",
            WatchKind::WaitingRedeem { .. } => "waiting_redeem",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WatchJob {
    pub match_id: String,
    pub chain: Chain,
    pub kind: WatchKind,
    pub htlc: (String, u32),
    pub not_before: u32,
    pub variants: Vec<String>,
    /// Set once a spend of `htlc` has confirmed.
    pub done: bool,
    /// The HTLC output `(value_sat, scriptpubkey_hex)`, recorded by the
    /// watchtower when it first broadcasts a spend of it. While set, a spend
    /// still in the mempool is taken to be ours and re-sent each tick (a
    /// dearer variant can replace it) until a spend confirms.
    #[serde(default)]
    pub sent_prevout: Option<(u64, String)>,
}

pub struct Store {
    db: DB,
    /// Serialises read-modify-write sequences (message seq numbers, strike lists).
    rmw: Mutex<()>,
    /// Serialises matcher operations so one offer can't be paired twice.
    matching: Mutex<()>,
}

fn lock(m: &Mutex<()>) -> MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Store {
    pub fn open(dir: &Path) -> anyhow::Result<Store> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let db = DB::open_cf(&opts, dir, ALL_CFS)
            .with_context(|| format!("opening swap db at {dir:?}"))?;
        Ok(Store { db, rmw: Mutex::new(()), matching: Mutex::new(()) })
    }

    /// Held by the matcher for the whole of each operation.
    pub(crate) fn lock_matching(&self) -> MutexGuard<'_, ()> {
        lock(&self.matching)
    }

    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db.cf_handle(name).expect("column family created at open")
    }

    fn put_json<T: Serialize>(&self, cf: &str, key: &str, v: &T) -> anyhow::Result<()> {
        self.db.put_cf(self.cf(cf), key.as_bytes(), serde_json::to_vec(v)?)?;
        Ok(())
    }

    fn get_json<T: DeserializeOwned>(&self, cf: &str, key: &str) -> Option<T> {
        let bytes = self.db.get_cf(self.cf(cf), key.as_bytes()).ok().flatten()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Every decodable `(key, value)` in `cf` whose key starts with `prefix`, in key order.
    fn scan<T: DeserializeOwned>(&self, cf: &str, prefix: &str) -> Vec<(String, T)> {
        self.db
            .iterator_cf(self.cf(cf), IteratorMode::From(prefix.as_bytes(), IterDirection::Forward))
            .map_while(Result::ok)
            .take_while(|(k, _)| k.starts_with(prefix.as_bytes()))
            .filter_map(|(k, v)| {
                Some((String::from_utf8(k.to_vec()).ok()?, serde_json::from_slice(&v).ok()?))
            })
            .collect()
    }

    fn delete(&self, cf: &str, key: &str) {
        let _ = self.db.delete_cf(self.cf(cf), key.as_bytes());
    }

    // ---- offers ----

    pub fn put_offer(&self, o: &Offer) -> anyhow::Result<()> {
        self.put_json(CF_OFFERS, &o.id, o)
    }

    pub fn offer(&self, id: &str) -> Option<Offer> {
        self.get_json(CF_OFFERS, id)
    }

    pub fn all_offers(&self) -> Vec<Offer> {
        self.scan(CF_OFFERS, "").into_iter().map(|(_, o)| o).collect()
    }

    /// `Open` offers in `direction`, oldest first. Includes private and
    /// not-yet-ticked expired offers; callers filter as they need.
    pub fn open_offers(&self, direction: Direction) -> Vec<Offer> {
        let mut v: Vec<Offer> = self
            .all_offers()
            .into_iter()
            .filter(|o| o.status == OfferStatus::Open && o.direction == direction)
            .collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    pub fn offer_by_code(&self, code: &str) -> Option<Offer> {
        self.all_offers().into_iter().find(|o| o.share_code.as_deref() == Some(code))
    }

    /// The offer `token` created with idempotency key `client_id`, if any.
    pub fn offer_by_client(&self, token: &str, client_id: &str) -> Option<Offer> {
        self.all_offers()
            .into_iter()
            .find(|o| o.token == token && o.client_id.as_deref() == Some(client_id))
    }

    // ---- matches ----

    pub fn put_match(&self, m: &Match) -> anyhow::Result<()> {
        self.put_json(CF_MATCHES, &m.id, m)
    }

    pub fn get_match(&self, id: &str) -> Option<Match> {
        self.get_json(CF_MATCHES, id)
    }

    pub fn all_matches(&self) -> Vec<Match> {
        self.scan(CF_MATCHES, "").into_iter().map(|(_, m)| m).collect()
    }

    /// Every match whose `chain` funding is the outpoint `txid:vout`. More
    /// than one is possible: funding outpoints are caller-supplied, so a
    /// squatter can claim another match's.
    pub fn matches_funded_by(&self, chain: Chain, txid: &str, vout: u32) -> Vec<Match> {
        self.all_matches()
            .into_iter()
            .filter(|m| {
                let funding = match chain {
                    Chain::Xbt => &m.xbt_funding,
                    Chain::Btc => &m.btc_funding,
                };
                funding.as_ref().is_some_and(|(t, v)| t == txid && *v == vout)
            })
            .collect()
    }

    // ---- message relay ----

    fn msg_prefix(match_id: &str) -> String {
        format!("{match_id}/")
    }

    /// Appends a message; returns its sequence number (1, 2, …).
    pub fn push_msg(&self, match_id: &str, from_role: &str, body: Value) -> anyhow::Result<u64> {
        let _g = lock(&self.rmw);
        let prefix = Self::msg_prefix(match_id);
        let last = self
            .scan::<Value>(CF_MSGS, &prefix)
            .last()
            .and_then(|(k, _)| k[prefix.len()..].parse::<u64>().ok())
            .unwrap_or(0);
        let seq = last + 1;
        let rec = serde_json::json!({ "from": from_role, "body": body });
        self.put_json(CF_MSGS, &format!("{prefix}{seq:020}"), &rec)?;
        Ok(seq)
    }

    /// Messages for `match_id` with sequence number greater than `seq`, in order.
    pub fn msgs_after(&self, match_id: &str, seq: u64) -> Vec<(u64, String, Value)> {
        let prefix = Self::msg_prefix(match_id);
        self.scan::<Value>(CF_MSGS, &prefix)
            .into_iter()
            .filter_map(|(k, mut v)| {
                let n = k[prefix.len()..].parse::<u64>().ok()?;
                let from = v.get("from")?.as_str()?.to_string();
                Some((n, from, v.get_mut("body")?.take()))
            })
            .filter(|(n, _, _)| *n > seq)
            .collect()
    }

    // ---- watchtower jobs ----

    fn watch_key(job: &WatchJob) -> String {
        format!("{}/{}/{}", job.match_id, job.chain.as_str(), job.kind.key_tag())
    }

    /// Insert or replace; one match can hold one job per (chain, kind).
    pub fn put_watch(&self, job: &WatchJob) -> anyhow::Result<()> {
        self.put_json(CF_WATCH, &Self::watch_key(job), job)
    }

    pub fn watch_jobs(&self) -> Vec<WatchJob> {
        self.scan(CF_WATCH, "").into_iter().map(|(_, j)| j).collect()
    }

    // ---- anti-griefing strikes ----

    fn strike_times(&self, token: &str) -> Vec<u64> {
        self.get_json(CF_STRIKES, token).unwrap_or_default()
    }

    pub fn add_strike(&self, token: &str, now: u64) -> anyhow::Result<()> {
        let _g = lock(&self.rmw);
        let mut times: Vec<u64> =
            self.strike_times(token).into_iter().filter(|t| in_window(*t, now)).collect();
        times.push(now);
        self.put_json(CF_STRIKES, token, &times)
    }

    /// Strikes against `token` within the last 24 h.
    pub fn strikes(&self, token: &str, now: u64) -> u32 {
        self.strike_times(token).into_iter().filter(|t| in_window(*t, now)).count() as u32
    }

    // ---- retention ----

    /// Drops closed/cancelled matches (with their messages and watch jobs) and
    /// cancelled/expired/filled offers more than 7 days old, plus strike lists
    /// with nothing left in the window. Offers still `Matched` go once their
    /// match is gone or closed. A match with any watch job not yet done is
    /// kept, jobs and all, however old: a refund can open long after the match
    /// finished.
    pub fn purge(&self, now: u64) {
        let old = |t: u64| now.saturating_sub(t) > PURGE_AFTER_SECS;
        let pending: std::collections::HashSet<String> =
            self.watch_jobs().into_iter().filter(|j| !j.done).map(|j| j.match_id).collect();

        for m in self.all_matches() {
            let finished = matches!(m.status, MatchStatus::Closed | MatchStatus::Cancelled);
            if finished && m.closed_at.is_some_and(old) && !pending.contains(&m.id) {
                for (k, _) in self.scan::<Value>(CF_MSGS, &Self::msg_prefix(&m.id)) {
                    self.delete(CF_MSGS, &k);
                }
                for (k, _) in self.scan::<Value>(CF_WATCH, &format!("{}/", m.id)) {
                    self.delete(CF_WATCH, &k);
                }
                self.delete(CF_MATCHES, &m.id);
            }
        }

        for o in self.all_offers() {
            let drop = match o.status {
                OfferStatus::Cancelled | OfferStatus::Expired | OfferStatus::Filled => true,
                OfferStatus::Matched => o
                    .match_id
                    .as_deref()
                    .and_then(|id| self.get_match(id))
                    .is_none_or(|m| m.status == MatchStatus::Closed),
                OfferStatus::Open => false,
            };
            if drop && old(o.expires_at) {
                self.delete(CF_OFFERS, &o.id);
            }
        }

        let _g = lock(&self.rmw);
        for (token, times) in self.scan::<Vec<u64>>(CF_STRIKES, "") {
            if !times.iter().any(|t| in_window(*t, now)) {
                self.delete(CF_STRIKES, &token);
            }
        }
    }
}

fn in_window(t: u64, now: u64) -> bool {
    t <= now && now - t < STRIKE_WINDOW_SECS
}

/// Generates a random 128-bit hex id.
pub fn new_id() -> anyhow::Result<String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| anyhow!("CSPRNG: {e}"))?;
    Ok(hex::encode(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store in its own temp dir; dropping it closes the store, then
    /// removes the dir.
    struct TempStore {
        store: Store,
        _dir: crate::test_tmp::TempDir,
    }

    impl std::ops::Deref for TempStore {
        type Target = Store;
        fn deref(&self) -> &Store {
            &self.store
        }
    }

    fn temp_store(label: &str) -> TempStore {
        let dir = crate::test_tmp::TempDir::new(&format!("swap-store-test-{label}"));
        TempStore { store: Store::open(&dir).unwrap(), _dir: dir }
    }

    #[test]
    fn msgs_are_sequenced_per_match() {
        let s = temp_store("msgs");
        assert_eq!(s.push_msg("m1", "initiator", serde_json::json!({"a": 1})).unwrap(), 1);
        assert_eq!(s.push_msg("m1", "participant", serde_json::json!({"a": 2})).unwrap(), 2);
        assert_eq!(s.push_msg("m10", "initiator", serde_json::json!(3)).unwrap(), 1);
        let all = s.msgs_after("m1", 0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1], (2, "participant".into(), serde_json::json!({"a": 2})));
        assert_eq!(s.msgs_after("m1", 1).len(), 1);
        assert!(s.msgs_after("m1", 2).is_empty());
    }

    #[test]
    fn watch_jobs_keyed_per_chain_and_kind() {
        let s = temp_store("watch");
        let job = |chain, kind| WatchJob {
            match_id: "m1".into(),
            chain,
            kind,
            htlc: ("ab".repeat(32), 0),
            not_before: 10,
            variants: vec!["00".into()],
            done: false,
            sent_prevout: None,
        };
        s.put_watch(&job(Chain::Xbt, WatchKind::Refund)).unwrap();
        s.put_watch(&job(Chain::Btc, WatchKind::Redeem)).unwrap();
        let waiting = WatchKind::WaitingRedeem {
            sigs_hex: vec!["01".into()],
            pubkey_hex: "02".into(),
            witness_script_hex: "03".into(),
        };
        s.put_watch(&job(Chain::Xbt, waiting.clone())).unwrap();
        let mut done = job(Chain::Xbt, WatchKind::Refund);
        done.done = true;
        s.put_watch(&done).unwrap();
        let jobs = s.watch_jobs();
        assert_eq!(jobs.len(), 3);
        assert!(jobs.contains(&done));
        assert!(jobs.iter().any(|j| j.kind == waiting));
    }

    #[test]
    fn swap_htlc_outpoints_are_recognised() {
        let s = temp_store("htlc");
        assert!(s.matches_funded_by(Chain::Xbt, "aa", 0).is_empty());
        let terms: SwapTerms = serde_json::from_value(serde_json::json!({
            "match_id": "m1", "network": "bitcoin", "xbt_sat": 1, "btc_sat": 1,
            "price_sat_per_xbt": 1, "fee_bps": 50, "fee_address": "f", "matched_at": 0,
            "xbt_locktime": 0, "btc_locktime": 0, "xbt_confs": 6, "btc_confs": 2,
            "accept_by": 0, "initiator_fund_by": 0
        }))
        .unwrap();
        let m = Match {
            id: "m1".into(),
            terms,
            initiator_offer: "a".into(),
            participant_offer: "b".into(),
            initiator_token: "ta".into(),
            participant_token: "tb".into(),
            initiator_secret_hash: [0; 32],
            participant_secret_hash: [0; 32],
            accepted: (true, true),
            status: MatchStatus::ParticipantFunded,
            xbt_funding: Some(("aa".into(), 1)),
            btc_funding: Some(("bb".into(), 0)),
            participant_fund_by: None,
            pre_match_amounts: (1, 1),
            closed_at: None,
            keys: (None, None),
            hashlock: None,
        };
        s.put_match(&m).unwrap();
        assert_eq!(s.matches_funded_by(Chain::Xbt, "aa", 1).into_iter().map(|m| m.id).collect::<Vec<_>>(), ["m1"]);
        assert_eq!(s.matches_funded_by(Chain::Btc, "bb", 0).into_iter().map(|m| m.id).collect::<Vec<_>>(), ["m1"]);
        assert!(s.matches_funded_by(Chain::Btc, "aa", 1).is_empty());
        assert!(s.matches_funded_by(Chain::Xbt, "bb", 0).is_empty());
        assert!(s.matches_funded_by(Chain::Xbt, "aa", 0).is_empty());
        assert!(s.matches_funded_by(Chain::Xbt, "cc", 0).is_empty());
    }

    #[test]
    fn purge_keeps_a_finished_match_while_a_watch_job_is_pending() {
        let dir = crate::test_tmp::TempDir::new("swap-store-test-purge");
        let s = Store::open(&dir).unwrap();
        let terms: SwapTerms = serde_json::from_value(serde_json::json!({
            "match_id": "m1", "network": "bitcoin", "xbt_sat": 1, "btc_sat": 1,
            "price_sat_per_xbt": 1, "fee_bps": 50, "fee_address": "f", "matched_at": 0,
            "xbt_locktime": 0, "btc_locktime": 0, "xbt_confs": 6, "btc_confs": 2,
            "accept_by": 0, "initiator_fund_by": 0
        }))
        .unwrap();
        let now = 2_000_000_000u64;
        let m = Match {
            id: "m1".into(),
            terms,
            initiator_offer: "a".into(),
            participant_offer: "b".into(),
            initiator_token: "ta".into(),
            participant_token: "tb".into(),
            initiator_secret_hash: [0; 32],
            participant_secret_hash: [0; 32],
            accepted: (true, true),
            status: MatchStatus::Cancelled,
            xbt_funding: Some(("aa".into(), 0)),
            btc_funding: None,
            participant_fund_by: None,
            pre_match_amounts: (1, 1),
            closed_at: Some(now - 8 * 24 * 3600),
            keys: (None, None),
            hashlock: None,
        };
        s.put_match(&m).unwrap();
        s.push_msg("m1", "initiator", serde_json::json!(1)).unwrap();
        let mut refund = WatchJob {
            match_id: "m1".into(),
            chain: Chain::Xbt,
            kind: WatchKind::Refund,
            htlc: ("aa".into(), 0),
            not_before: 10,
            variants: vec!["00".into()],
            done: false,
            sent_prevout: None,
        };
        s.put_watch(&refund).unwrap();
        let redeem = WatchJob { kind: WatchKind::Redeem, done: true, ..refund.clone() };
        s.put_watch(&redeem).unwrap();

        s.purge(now);
        assert!(s.get_match("m1").is_some(), "the XBT refund is still pending");
        assert_eq!(s.watch_jobs().len(), 2, "no job of the match goes while one is pending");
        assert_eq!(s.msgs_after("m1", 0).len(), 1);

        refund.done = true;
        s.put_watch(&refund).unwrap();
        s.purge(now);
        assert!(s.get_match("m1").is_none());
        assert!(s.watch_jobs().is_empty());
        assert!(s.msgs_after("m1", 0).is_empty());
    }

    #[test]
    fn serde_names() {
        assert_eq!(serde_json::to_value(Direction::XbtToBtc).unwrap(), "xbt_to_btc");
        assert_eq!(serde_json::to_value(Direction::BtcToXbt).unwrap(), "btc_to_xbt");
    }
}
