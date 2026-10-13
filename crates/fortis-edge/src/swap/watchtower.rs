//! The swap watchtower: a background thread that keeps swaps moving while
//! the phones are offline. Every [`TICK`] it
//! - runs the matcher's deadline housekeeping ([`matcher::tick`]), then
//!   pairs crossing offers left open together ([`matcher::rematch`]);
//! - starts the participant's funding clock once the initiator's XBT HTLC
//!   is really on chain (exact script and value) with enough confirmations;
//! - closes matches whose funded HTLCs are all spent;
//! - works the watch jobs phones handed it: refunds after their locktime,
//!   fully signed redeems, and presigned XBT redeems finished with the
//!   preimage the initiator reveals when redeeming the BTC HTLC.
//!
//! Chain access goes through [`ChainReader`]; [`UpstreamReader`] is the real
//! one, over the edge's own upstreams. Watchtower broadcasts go straight to
//! the upstream: the `/tx` service-fee check is for clients.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{ecdsa, Message, PublicKey, Secp256k1};
use bitcoin::{OutPoint, ScriptBuf, Transaction, TxOut};
use serde_json::{json, Value};
use tiny_http::Method;
use wallet_core::htlc::preimage_from_witness;
use wallet_core::sighash::sighash_all;
use wallet_core::swap_terms::participant_fund_latest;
use wallet_core::{finish_presigned_redeem, network_params, Chain, PresignedRedeem};

use super::store::{Match, MatchStatus, Store, WatchJob, WatchKind};
use super::{leg_witness_script, matcher, relay};
use crate::proxy::Upstream;

/// How often [`spawn`]'s loop runs [`run_once`] by default.
pub const TICK: Duration = Duration::from_secs(60);
/// The participant funds within this long of the XBT funding confirming, and
/// never after [`participant_fund_latest`].
const PARTICIPANT_FUND_SECS: u64 = 7200;

/// One unspent output, as `fortis-index`'s `GET /txout` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOutInfo {
    pub value_sat: u64,
    pub scriptpubkey_hex: String,
    pub confirmations: u32,
}

pub trait ChainReader: Send + Sync {
    /// The output if it exists and is unspent (mempool included: a spend
    /// still in the mempool counts); `None` if it is spent or unknown.
    fn txout(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>>;
    /// `txid`'s output `vout` as the transaction created it, spent or not
    /// (mempool included); `None` if the transaction is unknown or has no
    /// such output. `confirmations` is not read: always 0.
    fn created_output(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>>;
    /// The raw hex of the transaction spending `txid:vout`, if any. BTC only;
    /// always `Ok(None)` on XBT.
    fn spending_tx_hex(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<String>>;
    /// Whether `txid:vout` is spent by a *confirmed* transaction. A spend
    /// still in the mempool can be evicted, so it doesn't count.
    fn spend_confirmed(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<bool>;
    /// The block time (unix seconds) of the block that mined `txid`; `None`
    /// while it is unconfirmed or unknown. BTC only; always `Ok(None)` on XBT.
    fn tx_block_time(&self, chain: Chain, txid: &str) -> anyhow::Result<Option<u64>>;
    /// sat/vB for a 6-block target.
    fn feerate(&self, chain: Chain) -> anyhow::Result<u64>;
    /// Broadcasts `hex`; returns the txid.
    fn broadcast(&self, chain: Chain, hex: &str) -> anyhow::Result<String>;
    /// Unix seconds.
    fn now(&self) -> u64;
}

/// The swap config [`matcher::rematch`] builds terms with.
#[derive(Clone, Debug)]
pub struct Matching {
    pub max_btc_sat: u64,
    /// Terms network: "bitcoin" | "regtest".
    pub network: String,
    pub fee_address: String,
}

/// Runs [`run_once`] every `tick` ([`TICK`] in production) forever, logging
/// failures (panics included) without dying.
pub fn spawn(store: Arc<Store>, matching: Matching, r: Arc<dyn ChainReader>, tick: Duration) {
    std::thread::spawn(move || loop {
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_once(&store, &matching, &*r)
        }));
        if ok.is_err() {
            eprintln!("fortis-edge: swap watchtower pass panicked; retrying next tick");
        }
        std::thread::sleep(tick);
    });
}

/// Every watch job not yet done, once.
fn run_jobs(store: &Store, r: &dyn ChainReader, now: u64) {
    for job in store.watch_jobs().into_iter().filter(|j| !j.done) {
        let what = format!("{} {} {}", job.match_id, job.chain.as_str(), job.kind.key_tag());
        if let Err(e) = run_job(store, r, job, now) {
            eprintln!("fortis-edge: watchtower: job {what}: {e:#}");
        }
    }
}

/// One watchtower pass. Errors are per match / per job: logged, then the
/// next tick tries again.
pub fn run_once(store: &Store, matching: &Matching, r: &dyn ChainReader) {
    let now = r.now();
    // Refunds and redeems first: they move coins against deadlines, so
    // nothing else in the pass (matching takes the matching lock and reads
    // every offer) may hold them up.
    run_jobs(store, r, now);
    matcher::tick(store, now);
    matcher::rematch(store, now, matching.max_btc_sat, &matching.network, &matching.fee_address);
    for m in store.all_matches() {
        let id = m.id.clone();
        let res = match m.status {
            MatchStatus::InitiatorFunded if m.participant_fund_by.is_none() => {
                start_participant_clock(store, r, m, now)
            }
            _ => Ok(()),
        };
        if let Err(e) = res {
            eprintln!("fortis-edge: watchtower: deadline for match {id}: {e:#}");
        }
    }
    for m in store.all_matches() {
        let id = m.id.clone();
        if let Err(e) = close_if_spent(store, r, m, now) {
            eprintln!("fortis-edge: watchtower: closing match {id}: {e:#}");
        }
    }

    store.purge(now);
}

/// Sets `participant_fund_by` once the declared XBT funding outpoint pays
/// exactly the expected XBT-leg HTLC script and `xbt_sat`, with
/// `xbt_confs` confirmations, and relays a `deadline` message. A declared
/// outpoint that doesn't match never starts the clock, so the initiator
/// misses its own deadline instead.
fn start_participant_clock(
    store: &Store,
    r: &dyn ChainReader,
    m: Match,
    now: u64,
) -> anyhow::Result<()> {
    let Some((txid, vout)) = m.xbt_funding.clone() else {
        return Ok(());
    };
    // Without both parties' keys and the hashlock there is no script to check.
    let Some(ws) = leg_witness_script(&m, Chain::Xbt) else {
        return Ok(());
    };
    let Some(out) = r.txout(Chain::Xbt, &txid, vout)? else {
        return Ok(());
    };
    let spk = ScriptBuf::new_p2wsh(&ws.wscript_hash());
    if !out.scriptpubkey_hex.eq_ignore_ascii_case(&hex::encode(spk.as_bytes()))
        || out.value_sat != m.terms.xbt_sat
        || out.confirmations < m.terms.xbt_confs
    {
        return Ok(());
    }
    let latest = participant_fund_latest(&m.terms) as u64;
    let fund_by = (now + PARTICIPANT_FUND_SECS).min(latest) as u32;
    {
        let _g = store.lock_matching();
        let Some(mut cur) = store.get_match(&m.id) else {
            return Ok(());
        };
        if cur.status != MatchStatus::InitiatorFunded
            || cur.participant_fund_by.is_some()
            || cur.xbt_funding != m.xbt_funding
        {
            return Ok(());
        }
        cur.participant_fund_by = Some(fund_by);
        store.put_match(&cur)?;
    }
    swap_event!(m.id, "XBT funding {txid}:{vout} confirmed ({} confs); the participant must fund BTC by {fund_by}", out.confirmations);
    relay::push(store, &m.id, "edge", json!({ "type": "deadline", "participant_fund_by": fund_by }))?;
    Ok(())
}

/// A funded match whose funded HTLCs are all spent (or gone) is `Closed`.
fn close_if_spent(store: &Store, r: &dyn ChainReader, m: Match, now: u64) -> anyhow::Result<()> {
    if !matches!(m.status, MatchStatus::InitiatorFunded | MatchStatus::ParticipantFunded) {
        return Ok(());
    }
    let legs = [(Chain::Xbt, &m.xbt_funding), (Chain::Btc, &m.btc_funding)];
    let mut funded = legs.iter().filter_map(|(c, f)| f.as_ref().map(|f| (*c, f))).peekable();
    if funded.peek().is_none() {
        return Ok(());
    }
    for (chain, (txid, vout)) in funded {
        if r.txout(chain, txid, *vout)?.is_some() {
            return Ok(());
        }
    }
    let _g = store.lock_matching();
    let Some(mut cur) = store.get_match(&m.id) else {
        return Ok(());
    };
    if cur.status != m.status || cur.xbt_funding != m.xbt_funding || cur.btc_funding != m.btc_funding {
        return Ok(()); // moved on since we looked; next tick re-checks
    }
    cur.status = MatchStatus::Closed;
    cur.closed_at = Some(now);
    swap_event!(cur.id, "closed: every funded HTLC is spent");
    store.put_match(&cur)
}

/// One watch job. It is done only once a spend of its HTLC *confirms*: a
/// spend still in the mempool can be evicted (a low-fee refund, say). While
/// the HTLC is unspent the job's spend goes out once due; while it is spent
/// in the mempool after we sent ours, ours is re-sent each tick, so a dearer
/// variant replaces it (they signal RBF) when fees rise.
fn run_job(store: &Store, r: &dyn ChainReader, mut job: WatchJob, now: u64) -> anyhow::Result<()> {
    let (txid, vout) = job.htlc.clone();
    // `resend`: the HTLC is spent in the mempool, by what we sent before.
    let (out, resend) = match r.txout(job.chain, &txid, vout)? {
        Some(out) => (out, false),
        None if r.spend_confirmed(job.chain, &txid, vout)? => {
            job.done = true;
            swap_event!(job.match_id, "watch job {} on {} {txid}:{vout} done: its spend confirmed", kind_name(&job.kind), job.chain.as_str());
            return store.put_watch(&job);
        }
        None => match &job.sent_prevout {
            Some((value_sat, spk)) => {
                (TxOutInfo { value_sat: *value_sat, scriptpubkey_hex: spk.clone(), confirmations: 0 }, true)
            }
            None => match phone_sent_redeem(store, r, &job)? {
                // The phone's own redeem: ours to fee-bump from now on.
                Some(prevout) => {
                    job.sent_prevout = Some((prevout.value_sat, prevout.scriptpubkey_hex.clone()));
                    store.put_watch(&job)?;
                    (prevout, true)
                }
                // Someone else's unconfirmed spend: nothing of ours to re-send.
                None => return Ok(()),
            },
        },
    };
    let txs: Vec<Transaction> = match &job.kind {
        WatchKind::Refund => {
            if now < job.not_before as u64 {
                return Ok(());
            }
            decode_all(&job.variants)?
        }
        WatchKind::Redeem => {
            let Some(m) = store.get_match(&job.match_id) else {
                return Ok(());
            };
            let need = match job.chain {
                Chain::Xbt => m.terms.xbt_confs,
                Chain::Btc => m.terms.btc_confs,
            };
            if !resend && out.confirmations < need {
                return Ok(());
            }
            decode_all(&job.variants)?
        }
        WatchKind::WaitingRedeem { sigs_hex, pubkey_hex, witness_script_hex } => {
            let Some(m) = store.get_match(&job.match_id) else {
                return Ok(());
            };
            // The preimage shows up when the counterparty redeems the other leg.
            let other = match job.chain {
                Chain::Xbt => (Chain::Btc, &m.btc_funding),
                Chain::Btc => (Chain::Xbt, &m.xbt_funding),
            };
            let (other_chain, Some((otxid, ovout))) = other else {
                return Ok(());
            };
            let Some(spender_hex) = r.spending_tx_hex(other_chain, otxid, *ovout)? else {
                return Ok(());
            };
            let spender = decode(&spender_hex)?;
            let other_op = OutPoint { txid: otxid.parse()?, vout: *ovout };
            let Some(hashlock) = m.hashlock.as_deref().and_then(match_hashlock) else {
                return Ok(()); // no hashlock relayed: nothing to open
            };
            let Some(preimage) = spender
                .input
                .iter()
                .find(|i| i.previous_output == other_op)
                .and_then(|i| preimage_from_witness(&i.witness, &hashlock))
            else {
                return Ok(()); // not spent yet, or refunded: no preimage
            };
            let (btc_params, xbt_params) =
                network_params(&m.terms.network).map_err(|e| anyhow!("{e}"))?;
            let params = match job.chain {
                Chain::Xbt => xbt_params,
                Chain::Btc => btc_params,
            };
            let prevout = TxOut {
                value: bitcoin::Amount::from_sat(out.value_sat),
                script_pubkey: ScriptBuf::from(hex::decode(&out.scriptpubkey_hex)?),
            };
            if sigs_hex.len() != job.variants.len() {
                bail!("{} sigs for {} variants", sigs_hex.len(), job.variants.len());
            }
            let finished: Vec<Transaction> = job
                .variants
                .iter()
                .zip(sigs_hex)
                .filter_map(|(tx_hex, sig_hex)| {
                    let p = PresignedRedeem {
                        tx_hex: tx_hex.clone(),
                        sig_hex: sig_hex.clone(),
                        pubkey_hex: pubkey_hex.clone(),
                        witness_script_hex: witness_script_hex.clone(),
                    };
                    finish_presigned_redeem(&p, &preimage).ok()
                })
                // A bad signature would make that variant unspendable.
                .filter(|tx| signature_covers(tx, &prevout, params.sighash_variant()))
                .collect();
            if finished.is_empty() {
                bail!("preimage found, but no variant is validly signed");
            }
            finished
        }
    };
    let feerate = r.feerate(job.chain).unwrap_or_else(|e| {
        eprintln!("fortis-edge: watchtower: {} feerate: {e:#}; using the highest variant", job.chain.as_str());
        u64::MAX
    });
    let tx = pick_variant(&txs, out.value_sat, feerate);
    let sent = r.broadcast(job.chain, &hex::encode(bitcoin::consensus::serialize(tx)))?;
    swap_event!(
        job.match_id, "watchtower sent the {} of {} {txid}:{vout}: {sent}{}",
        kind_name(&job.kind), job.chain.as_str(), if resend { " (re-sent)" } else { "" }
    );
    if job.sent_prevout.is_none() {
        job.sent_prevout = Some((out.value_sat, out.scriptpubkey_hex));
        store.put_watch(&job)?;
    }
    Ok(())
}

/// A match's relayed hashlock (hex, byte order as `hex::encode` wrote it).
fn match_hashlock(hex_str: &str) -> Option<sha256::Hash> {
    let bytes: [u8; 32] = hex::decode(hex_str).ok()?.try_into().ok()?;
    Some(sha256::Hash::from_byte_array(bytes))
}

/// A watch job's kind, for the journal.
pub(super) fn kind_name(k: &WatchKind) -> &'static str {
    match k {
        WatchKind::Refund => "refund",
        WatchKind::Redeem => "redeem",
        WatchKind::WaitingRedeem { .. } => "presigned redeem",
    }
}

/// For a BTC `Redeem` job whose HTLC is spent in the mempool by something
/// the job never sent: if that spend reveals this match's preimage, it is
/// the initiator's own redeem (sent by its phone), and the HTLC's prevout is
/// returned so the job can keep re-sending its variants at the current
/// feerate — a redeem stalled until the BTC locktime would otherwise lose
/// to the participant's refund. `None` for anything else (a refund).
fn phone_sent_redeem(store: &Store, r: &dyn ChainReader, job: &WatchJob) -> anyhow::Result<Option<TxOutInfo>> {
    if job.chain != Chain::Btc || !matches!(job.kind, WatchKind::Redeem) {
        return Ok(None);
    }
    let Some(m) = store.get_match(&job.match_id) else {
        return Ok(None);
    };
    let (Some(hashlock), Some(ws)) = (m.hashlock.as_deref(), leg_witness_script(&m, Chain::Btc)) else {
        return Ok(None);
    };
    let Some(spender_hex) = r.spending_tx_hex(Chain::Btc, &job.htlc.0, job.htlc.1)? else {
        return Ok(None);
    };
    let htlc_op = OutPoint { txid: job.htlc.0.parse()?, vout: job.htlc.1 };
    let Some(hashlock) = match_hashlock(hashlock) else {
        return Ok(None);
    };
    let revealed = decode(&spender_hex)?
        .input
        .iter()
        .find(|i| i.previous_output == htlc_op)
        .and_then(|i| preimage_from_witness(&i.witness, &hashlock));
    if revealed.is_none() {
        return Ok(None);
    }
    Ok(Some(TxOutInfo {
        value_sat: m.terms.btc_sat,
        scriptpubkey_hex: hex::encode(ScriptBuf::new_p2wsh(&ws.wscript_hash()).as_bytes()),
        confirmations: 0,
    }))
}

/// Whether a finished redeem's witness signature `[sig, pubkey, _, _, ws]`
/// is valid for its own input 0 spending `prevout`.
fn signature_covers(tx: &Transaction, prevout: &TxOut, variant: wallet_core::sighash::SighashVariant) -> bool {
    let check = || -> Option<bool> {
        let w = &tx.input.first()?.witness;
        let (sig, pk, ws) = (w.nth(0)?, w.nth(1)?, w.nth(4)?);
        let (flag, der) = sig.split_last()?;
        let ws = ScriptBuf::from(ws.to_vec());
        let sh = sighash_all(tx, std::slice::from_ref(prevout), 0, &ws, variant).ok()?;
        if *flag != sh.flag {
            return Some(false);
        }
        let sig = ecdsa::Signature::from_der(der).ok()?;
        let pk = PublicKey::from_slice(pk).ok()?;
        Some(Secp256k1::verification_only().verify_ecdsa(&Message::from_digest(sh.message), &sig, &pk).is_ok())
    };
    check().unwrap_or(false)
}

fn decode(hex_str: &str) -> anyhow::Result<Transaction> {
    let bytes = hex::decode(hex_str.trim()).context("tx hex")?;
    bitcoin::consensus::deserialize(&bytes).context("tx decode")
}

fn decode_all(variants: &[String]) -> anyhow::Result<Vec<Transaction>> {
    let txs: Vec<Transaction> = variants.iter().map(|v| decode(v)).collect::<anyhow::Result<_>>()?;
    if txs.is_empty() {
        bail!("no variants");
    }
    Ok(txs)
}

fn fee(tx: &Transaction, prevout_value: u64) -> u64 {
    let out: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    prevout_value.saturating_sub(out)
}

/// The cheapest variant paying at least `feerate` sat/vB, else the one
/// paying the most. `variants` must not be empty.
pub fn pick_variant(variants: &[Transaction], prevout_value: u64, feerate: u64) -> &Transaction {
    let meets = |tx: &&Transaction| fee(tx, prevout_value) / (tx.vsize() as u64).max(1) >= feerate;
    variants
        .iter()
        .filter(meets)
        .min_by_key(|tx| fee(tx, prevout_value))
        .or_else(|| variants.iter().max_by_key(|tx| fee(tx, prevout_value)))
        .expect("pick_variant needs at least one variant")
}

// ---- the real chain reader ----

/// [`ChainReader`] over the edge's own upstreams: `fortis-index` for XBT;
/// the Esplora chain (primary then fallbacks) for BTC, with the BTC node
/// (`--btc-rpc-url`) first for tip, fees and broadcast when configured.
pub struct UpstreamReader {
    xbt: Option<Upstream>,
    btc: Vec<Upstream>,
    btc_rpc: Option<fortis_node::Rpc>,
    /// Added to the wall clock; 0 except in tests (`--swap-clock-offset-secs`).
    clock_offset: u64,
}

/// What one GET across a fallback chain came to.
enum Got {
    Body(Vec<u8>),
    /// Every upstream answered 404.
    NotFound(String),
}

impl UpstreamReader {
    pub fn new(xbt: Option<Upstream>, btc: Vec<Upstream>, btc_rpc: Option<fortis_node::Rpc>) -> Self {
        UpstreamReader { xbt, btc, btc_rpc, clock_offset: 0 }
    }

    /// Test only: run the watchtower's clock `secs` ahead of the wall clock,
    /// so an end-to-end test can reach refund times without waiting.
    pub fn with_clock_offset(mut self, secs: u64) -> Self {
        self.clock_offset = secs;
        self
    }

    fn upstreams(&self, chain: Chain) -> anyhow::Result<&[Upstream]> {
        let ups = match chain {
            Chain::Xbt => self.xbt.as_slice(),
            Chain::Btc => self.btc.as_slice(),
        };
        if ups.is_empty() {
            bail!("no {} upstream", chain.as_str());
        }
        Ok(ups)
    }

    /// GET `rest` from each upstream in turn until one answers 200. Only
    /// when all of them answer 404 is the thing reported missing: one
    /// misrouting provider's 404 must not read as "spent".
    fn get(&self, chain: Chain, rest: &str) -> anyhow::Result<Got> {
        let mut not_found = None;
        let mut last_err = None;
        for up in self.upstreams(chain)? {
            match up.forward(&Method::Get, rest, "", &[]) {
                Ok(r) if r.status == 200 => return Ok(Got::Body(r.body)),
                Ok(r) if r.status == 404 => {
                    not_found = Some(String::from_utf8_lossy(&r.body).into_owned())
                }
                Ok(r) => last_err = Some(anyhow!("{} {rest}: status {}", chain.as_str(), r.status)),
                Err(e) => last_err = Some(e.context(format!("{} {rest}", chain.as_str()))),
            }
        }
        match (last_err, not_found) {
            (None, Some(body)) => Ok(Got::NotFound(body)),
            (Some(e), _) => Err(e),
            (None, None) => Err(anyhow!("{} {rest}: no answer", chain.as_str())),
        }
    }

    fn get_json(&self, chain: Chain, rest: &str) -> anyhow::Result<Option<Value>> {
        match self.get(chain, rest)? {
            Got::Body(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("{rest}: bad JSON"))?)),
            Got::NotFound(_) => Ok(None),
        }
    }

    fn btc_tip(&self) -> anyhow::Result<u64> {
        if let Some(rpc) = &self.btc_rpc {
            if let Some(h) = rpc.call("getblockcount", json!([])).ok().and_then(|v| v.as_u64()) {
                return Ok(h);
            }
        }
        match self.get(Chain::Btc, "blocks/tip/height")? {
            Got::Body(b) => String::from_utf8_lossy(&b).trim().parse().context("tip height"),
            Got::NotFound(_) => bail!("btc tip height: not found"),
        }
    }

    fn btc_txout(&self, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>> {
        let Some(tx) = self.get_json(Chain::Btc, &format!("tx/{txid}"))? else {
            return Ok(None);
        };
        let Some(o) = tx["vout"].get(vout as usize) else {
            return Ok(None);
        };
        let Some(spend) = self.get_json(Chain::Btc, &format!("tx/{txid}/outspend/{vout}"))? else {
            bail!("btc outspend {txid}:{vout}: not found");
        };
        if spend["spent"].as_bool() != Some(false) {
            return Ok(None);
        }
        let confirmations = if tx["status"]["confirmed"].as_bool() == Some(true) {
            let height = tx["status"]["block_height"].as_u64().context("block_height")?;
            (self.btc_tip()? + 1).saturating_sub(height) as u32
        } else {
            0
        };
        Ok(Some(TxOutInfo {
            value_sat: o["value"].as_u64().context("vout value")?,
            scriptpubkey_hex: o["scriptpubkey"].as_str().context("vout scriptpubkey")?.to_string(),
            confirmations,
        }))
    }

    fn xbt_txout(&self, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>> {
        let v = match self.get(Chain::Xbt, &format!("txout/{txid}/{vout}"))? {
            Got::Body(b) => serde_json::from_slice::<Value>(&b).context("txout: bad JSON")?,
            // fortis-index's own "spent or unknown"; any other 404 (an index
            // without the route) is an error, not a spend.
            Got::NotFound(body) if body.contains("spent or unknown") => return Ok(None),
            Got::NotFound(body) => bail!("xbt txout: 404 {body}"),
        };
        Ok(Some(TxOutInfo {
            value_sat: v["value_sat"].as_u64().context("value_sat")?,
            scriptpubkey_hex: v["scriptpubkey_hex"].as_str().context("scriptpubkey_hex")?.to_string(),
            confirmations: v["confirmations"].as_u64().unwrap_or(0) as u32,
        }))
    }

    /// POST the tx to each upstream until one takes it; a definite rejection
    /// (any 4xx but 403/429) stops there.
    fn post_tx(&self, chain: Chain, hex: &str) -> anyhow::Result<String> {
        let mut last_err = None;
        for up in self.upstreams(chain)? {
            match up.forward(&Method::Post, "tx", "", hex.as_bytes()) {
                Ok(r) if r.status == 200 => return Ok(String::from_utf8_lossy(&r.body).trim().to_string()),
                Ok(r) => {
                    let e = anyhow!("{} broadcast: {} {}", chain.as_str(), r.status, String::from_utf8_lossy(&r.body));
                    if r.status != 403 && r.status != 429 && r.status < 500 {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("{} broadcast: no upstream", chain.as_str())))
    }
}

impl ChainReader for UpstreamReader {
    fn txout(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>> {
        match chain {
            Chain::Xbt => self.xbt_txout(txid, vout),
            Chain::Btc => self.btc_txout(txid, vout),
        }
    }

    // Both indexes serve Esplora's `GET /tx/:txid`.
    fn created_output(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>> {
        let Some(tx) = self.get_json(chain, &format!("tx/{txid}"))? else {
            return Ok(None);
        };
        if tx["txid"].as_str() != Some(txid) {
            bail!("{} tx {txid}: the upstream answered with another transaction", chain.as_str());
        }
        let Some(o) = tx["vout"].get(vout as usize) else {
            return Ok(None);
        };
        Ok(Some(TxOutInfo {
            value_sat: o["value"].as_u64().context("vout value")?,
            scriptpubkey_hex: o["scriptpubkey"].as_str().context("vout scriptpubkey")?.to_string(),
            confirmations: 0,
        }))
    }

    fn spend_confirmed(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<bool> {
        match chain {
            // fortis-index's confirmed-UTXO-set view: the output stays until
            // its spend is mined.
            Chain::Xbt => match self.get(Chain::Xbt, &format!("txout/{txid}/{vout}?mempool=false"))? {
                Got::Body(_) => Ok(false),
                Got::NotFound(body) if body.contains("spent or unknown") => Ok(true),
                Got::NotFound(body) => bail!("xbt txout: 404 {body}"),
            },
            Chain::Btc => {
                let spend = self
                    .get_json(Chain::Btc, &format!("tx/{txid}/outspend/{vout}"))?
                    .with_context(|| format!("btc outspend {txid}:{vout}: not found"))?;
                Ok(spend["spent"].as_bool() == Some(true) && spend["status"]["confirmed"].as_bool() == Some(true))
            }
        }
    }

    fn spending_tx_hex(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<String>> {
        if chain == Chain::Xbt {
            return Ok(None);
        }
        let Some(spend) = self.get_json(Chain::Btc, &format!("tx/{txid}/outspend/{vout}"))? else {
            return Ok(None);
        };
        let Some(spender) = spend["txid"].as_str().filter(|_| spend["spent"].as_bool() == Some(true)) else {
            return Ok(None);
        };
        match self.get(Chain::Btc, &format!("tx/{spender}/hex"))? {
            Got::Body(b) => Ok(Some(String::from_utf8_lossy(&b).trim().to_string())),
            Got::NotFound(_) => Ok(None),
        }
    }

    fn tx_block_time(&self, chain: Chain, txid: &str) -> anyhow::Result<Option<u64>> {
        if chain == Chain::Xbt {
            return Ok(None);
        }
        if let Some(rpc) = &self.btc_rpc {
            // `blocktime` is absent while the tx is in the mempool.
            if let Ok(v) = rpc.call("getrawtransaction", json!([txid, 1])) {
                return Ok(v["blocktime"].as_u64());
            }
        }
        let Some(tx) = self.get_json(Chain::Btc, &format!("tx/{txid}"))? else {
            return Ok(None);
        };
        if tx["status"]["confirmed"].as_bool() != Some(true) {
            return Ok(None);
        }
        Ok(Some(tx["status"]["block_time"].as_u64().context("block_time")?))
    }

    fn feerate(&self, chain: Chain) -> anyhow::Result<u64> {
        match chain {
            Chain::Xbt => {
                let v = self.get_json(Chain::Xbt, "v1/fees/recommended")?.context("xbt fees: not found")?;
                Ok(v["hourFee"].as_u64().context("hourFee")?.max(1))
            }
            Chain::Btc => {
                if let Some(rate) = self.btc_rpc.as_ref().and_then(|rpc| fortis_node::estimate_feerate(rpc, 6).ok()) {
                    return Ok(rate.max(1));
                }
                let v = self.get_json(Chain::Btc, "fee-estimates")?.context("btc fee-estimates: not found")?;
                Ok((v["6"].as_f64().context("fee-estimates[6]")?.ceil() as u64).max(1))
            }
        }
    }

    fn broadcast(&self, chain: Chain, hex: &str) -> anyhow::Result<String> {
        if chain == Chain::Btc {
            if let Some(rpc) = &self.btc_rpc {
                match fortis_node::broadcast(rpc, hex) {
                    Ok(txid) => return Ok(txid.to_string()),
                    // The node is up: it genuinely rejected the tx.
                    Err(e) if rpc.call("getblockcount", json!([])).is_ok() => {
                        return Err(e.context("btc node rejected the transaction"))
                    }
                    Err(e) => eprintln!("fortis-edge: watchtower: BTC node unreachable ({e}); using the upstream"),
                }
            }
        }
        self.post_tx(chain, hex)
    }

    fn now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) + self.clock_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::{absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence};
    use bitcoin::{Transaction, TxIn, TxOut, Txid, Witness};
    use wallet_core::{network_params, HtlcContract, SwapKey, SwapTerms};

    use crate::swap::store::{
        Direction, Match, MatchStatus, Offer, OfferStatus, Store, WatchJob, WatchKind,
    };

    const NOW: u64 = 1_900_000_000;
    const PREIMAGE: [u8; 32] = [7; 32];

    #[derive(Default)]
    struct FakeChain {
        txouts: Mutex<HashMap<(Chain, String, u32), TxOutInfo>>,
        spenders: Mutex<HashMap<(String, u32), String>>,
        /// Outpoints whose spend is confirmed (not just in the mempool).
        confirmed_spends: Mutex<std::collections::HashSet<(Chain, String, u32)>>,
        feerate: Mutex<u64>,
        now: Mutex<u64>,
        broadcasts: Mutex<Vec<String>>,
    }

    impl FakeChain {
        fn set_txout(&self, chain: Chain, at: &(String, u32), info: Option<TxOutInfo>) {
            let key = (chain, at.0.clone(), at.1);
            let mut m = self.txouts.lock().unwrap();
            match info {
                Some(i) => m.insert(key, i),
                None => m.remove(&key),
            };
        }
        /// The output is gone and its spend is confirmed.
        fn spend_confirms(&self, chain: Chain, at: &(String, u32)) {
            self.set_txout(chain, at, None);
            self.confirmed_spends.lock().unwrap().insert((chain, at.0.clone(), at.1));
        }
        fn set_now(&self, t: u64) {
            *self.now.lock().unwrap() = t;
        }
        fn broadcasts(&self) -> Vec<String> {
            self.broadcasts.lock().unwrap().clone()
        }
    }

    impl ChainReader for FakeChain {
        fn txout(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<TxOutInfo>> {
            Ok(self.txouts.lock().unwrap().get(&(chain, txid.to_string(), vout)).cloned())
        }
        fn created_output(&self, _: Chain, _: &str, _: u32) -> anyhow::Result<Option<TxOutInfo>> {
            anyhow::bail!("not in these tests")
        }
        fn spending_tx_hex(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<Option<String>> {
            if chain == Chain::Xbt {
                return Ok(None);
            }
            Ok(self.spenders.lock().unwrap().get(&(txid.to_string(), vout)).cloned())
        }
        fn spend_confirmed(&self, chain: Chain, txid: &str, vout: u32) -> anyhow::Result<bool> {
            Ok(self.confirmed_spends.lock().unwrap().contains(&(chain, txid.to_string(), vout)))
        }
        fn tx_block_time(&self, _chain: Chain, _txid: &str) -> anyhow::Result<Option<u64>> {
            Ok(None)
        }
        fn feerate(&self, _chain: Chain) -> anyhow::Result<u64> {
            Ok(*self.feerate.lock().unwrap())
        }
        fn broadcast(&self, _chain: Chain, hex: &str) -> anyhow::Result<String> {
            self.broadcasts.lock().unwrap().push(hex.to_string());
            Ok("txid".into())
        }
        fn now(&self) -> u64 {
            *self.now.lock().unwrap()
        }
    }

    /// A temp store: dropping it closes the store, then removes its RocksDB dir.
    struct TempStore {
        store: Store,
        _dir: crate::test_tmp::TempDir,
    }

    fn temp_store(label: &str) -> TempStore {
        let dir = crate::test_tmp::TempDir::new(&format!("watchtower-test-{label}"));
        TempStore { store: Store::open(&dir).unwrap(), _dir: dir }
    }

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

    fn keys_hex(p: &Party) -> (String, String) {
        (hex::encode(p.xbt.pubkey().serialize()), hex::encode(p.btc.pubkey().serialize()))
    }

    fn terms() -> SwapTerms {
        serde_json::from_value(serde_json::json!({
            "match_id": "m1", "network": "bitcoin", "xbt_sat": 10_000_000, "btc_sat": 100_000,
            "price_sat_per_xbt": 1_000_000, "fee_bps": 50, "fee_address": "f",
            "matched_at": NOW, "xbt_locktime": NOW + 48 * 3600, "btc_locktime": NOW + 24 * 3600,
            "xbt_confs": 6, "btc_confs": 2, "accept_by": NOW + 120, "initiator_fund_by": NOW + 1920,
            "contract_version": wallet_core::htlc::CURRENT_CONTRACT
        }))
        .unwrap()
    }

    fn outpoint(byte: u8, vout: u32) -> (String, u32) {
        (Txid::from_byte_array([byte; 32]).to_string(), vout)
    }

    fn op(at: &(String, u32)) -> OutPoint {
        OutPoint { txid: at.0.parse().unwrap(), vout: at.1 }
    }

    /// A match between `party(1)` (initiator) and `party(3)` (participant)
    /// with keys and hashlock relayed.
    fn a_match(status: MatchStatus, xbt: Option<(String, u32)>, btc: Option<(String, u32)>) -> Match {
        Match {
            id: "m1".into(),
            terms: terms(),
            initiator_offer: "a".into(),
            participant_offer: "b".into(),
            initiator_token: "ta".into(),
            participant_token: "tb".into(),
            initiator_secret_hash: [0; 32],
            participant_secret_hash: [0; 32],
            accepted: (true, true),
            status,
            xbt_funding: xbt,
            btc_funding: btc,
            participant_fund_by: None,
            pre_match_amounts: (10_000_000, 100_000),
            closed_at: None,
            keys: (Some(keys_hex(&party(1))), Some(keys_hex(&party(3)))),
            hashlock: Some(hex::encode(sha256::Hash::hash(&PREIMAGE).to_byte_array())),
            missed_participant_deadline: false,
            release_txs: None,
            release_sigs: None,
        }
    }

    fn xbt_leg() -> HtlcContract {
        let t = terms();
        let (_, xbt) = network_params(&t.network).unwrap();
        let (redeem, refund) = (party(3).xbt.pubkey(), party(1).xbt.pubkey());
        let h = sha256::Hash::hash(&PREIMAGE);
        HtlcContract::build(&xbt, wallet_core::htlc::CURRENT_CONTRACT, h, &redeem, &refund, t.xbt_locktime, Amount::from_sat(t.xbt_sat))
    }

    fn btc_leg() -> HtlcContract {
        let t = terms();
        let (btc, _) = network_params(&t.network).unwrap();
        let (redeem, refund) = (party(1).btc.pubkey(), party(3).btc.pubkey());
        let h = sha256::Hash::hash(&PREIMAGE);
        HtlcContract::build(&btc, wallet_core::htlc::CURRENT_CONTRACT, h, &redeem, &refund, t.btc_locktime, Amount::from_sat(t.btc_sat))
    }

    fn info(c: &HtlcContract, confirmations: u32) -> TxOutInfo {
        TxOutInfo {
            value_sat: c.value.to_sat(),
            scriptpubkey_hex: hex::encode(c.script_pubkey.as_bytes()),
            confirmations,
        }
    }

    fn tx_hex(tx: &Transaction) -> String {
        hex::encode(bitcoin::consensus::serialize(tx))
    }

    fn decode(h: &str) -> Transaction {
        bitcoin::consensus::deserialize(&hex::decode(h).unwrap()).unwrap()
    }

    fn signed_refund(c: &HtlcContract, at: OutPoint, key: &SwapKey, fee: u64) -> Transaction {
        let mut tx = c.refund_tx(at, ScriptBuf::new(), Amount::from_sat(fee)).unwrap();
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

    fn job(chain: Chain, kind: WatchKind, htlc: &(String, u32), not_before: u32, variants: Vec<String>) -> WatchJob {
        WatchJob { match_id: "m1".into(), chain, kind, htlc: htlc.clone(), not_before, variants, done: false, sent_prevout: None }
    }

    fn only_job(s: &Store) -> WatchJob {
        let jobs = s.watch_jobs();
        assert_eq!(jobs.len(), 1);
        jobs.into_iter().next().unwrap()
    }

    fn matching() -> Matching {
        Matching { max_btc_sat: 1_000_000, network: "bitcoin".into(), fee_address: "bc1qfee".into() }
    }

    fn open_offer(id: &str, direction: Direction, amount_sat: u64, price: u64, created_at: u64) -> Offer {
        Offer {
            id: id.into(),
            token: format!("tok-{id}"),
            secret_hash: [1; 32],
            direction,
            amount_sat,
            price_sat_per_xbt: price,
            created_at,
            expires_at: created_at + 86_400,
            private: false,
            share_code: None,
            status: OfferStatus::Open,
            match_id: None,
            client_id: None,
            recovery_tag: None,
        }
    }

    #[test]
    fn a_pass_rematches_crossing_open_offers() {
        let t = temp_store("rematch");
        let s = &t.store;
        s.put_offer(&open_offer("a", Direction::XbtToBtc, 20_000_000, 990_000, NOW - 20)).unwrap();
        s.put_offer(&open_offer("b", Direction::BtcToXbt, 200_000, 1_000_000, NOW - 10)).unwrap();
        run_once(s, &matching(), &chain_at(NOW));
        let ms = s.all_matches();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].terms.price_sat_per_xbt, 990_000);
        assert_eq!(ms[0].terms.fee_address, "bc1qfee");
    }

    fn chain_at(now: u64) -> FakeChain {
        let c = FakeChain::default();
        c.set_now(now);
        *c.feerate.lock().unwrap() = 1;
        c
    }

    #[test]
    fn refund_waits_for_not_before_then_broadcasts_once_unspent() {
        let t = temp_store("refund");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        s.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
        let c = xbt_leg();
        let refund = tx_hex(&signed_refund(&c, op(&xf), &party(1).xbt, 1_000));
        let not_before = terms().xbt_locktime;
        s.put_watch(&job(Chain::Xbt, WatchKind::Refund, &xf, not_before, vec![refund.clone()])).unwrap();

        let r = chain_at(not_before as u64 - 1);
        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 10)));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty(), "too early");
        assert!(!only_job(s).done);

        r.set_now(not_before as u64);
        run_once(s, &matching(), &r);
        assert_eq!(r.broadcasts(), [refund]);
        assert!(!only_job(s).done, "still unspent: keep watching");

        r.spend_confirms(Chain::Xbt, &xf);
        run_once(s, &matching(), &r);
        assert_eq!(r.broadcasts().len(), 1, "nothing more once spent");
        assert!(only_job(s).done);
    }

    #[test]
    fn refund_skipped_when_htlc_already_spent() {
        let t = temp_store("refund-spent");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        let mut m = a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(outpoint(0xbb, 1)));
        m.status = MatchStatus::Closed;
        s.put_match(&m).unwrap();
        let c = xbt_leg();
        let refund = tx_hex(&signed_refund(&c, op(&xf), &party(1).xbt, 1_000));
        s.put_watch(&job(Chain::Xbt, WatchKind::Refund, &xf, 0, vec![refund])).unwrap();

        let r = chain_at(NOW + 200 * 3600);
        r.spend_confirms(Chain::Xbt, &xf);
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty());
        assert!(only_job(s).done);
    }

    #[test]
    fn waiting_redeem_finishes_with_preimage_from_btc_redeem() {
        let t = temp_store("waiting");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();

        // The participant presigns its redeem of the XBT HTLC.
        let xc = xbt_leg();
        let part = party(3);
        let unsigned = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(2_000)).unwrap();
        let sig = part.xbt.sign(&xc.spend_sighash(&unsigned, 0).unwrap());
        let kind = WatchKind::WaitingRedeem {
            sigs_hex: vec![hex::encode(&sig)],
            pubkey_hex: hex::encode(part.xbt.pubkey().serialize()),
            witness_script_hex: hex::encode(xc.witness_script.as_bytes()),
        };
        s.put_watch(&job(Chain::Xbt, kind, &xf, 0, vec![tx_hex(&unsigned)])).unwrap();

        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xc, 8)));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty(), "no preimage on chain yet");

        // The initiator redeems the BTC HTLC, revealing the preimage.
        let bc = btc_leg();
        let btc_redeem = signed_redeem(&bc, op(&bf), &party(1).btc);
        r.spenders.lock().unwrap().insert(bf.clone(), tx_hex(&btc_redeem));
        run_once(s, &matching(), &r);
        let sent = r.broadcasts();
        assert_eq!(sent.len(), 1);
        let tx = decode(&sent[0]);
        assert_eq!(tx.input[0].previous_output, op(&xf));
        assert_eq!(tx.output, unsigned.output);
        let w = &tx.input[0].witness;
        assert_eq!(w.len(), 5);
        assert_eq!(w.nth(0).unwrap(), &sig[..]);
        assert_eq!(w.nth(2).unwrap(), &PREIMAGE[..]);
        assert_eq!(w.nth(4).unwrap(), xc.witness_script.as_bytes());
        assert!(!only_job(s).done);

        r.spend_confirms(Chain::Xbt, &xf);
        run_once(s, &matching(), &r);
        assert!(only_job(s).done);
    }

    #[test]
    fn waiting_redeem_skips_variants_its_signature_does_not_cover() {
        let t = temp_store("waiting-sig");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let xc = xbt_leg();
        let part = party(3);
        let signed = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(2_000)).unwrap();
        let other = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(50_000)).unwrap();
        // `other`'s slot carries `signed`'s signature: it can't verify.
        let sig = hex::encode(part.xbt.sign(&xc.spend_sighash(&signed, 0).unwrap()));
        let kind = WatchKind::WaitingRedeem {
            sigs_hex: vec![sig.clone(), sig],
            pubkey_hex: hex::encode(part.xbt.pubkey().serialize()),
            witness_script_hex: hex::encode(xc.witness_script.as_bytes()),
        };
        s.put_watch(&job(Chain::Xbt, kind, &xf, 0, vec![tx_hex(&other), tx_hex(&signed)])).unwrap();
        let r = chain_at(NOW + 3 * 3600);
        *r.feerate.lock().unwrap() = 100; // only `other` would meet it
        r.set_txout(Chain::Xbt, &xf, Some(info(&xc, 8)));
        let btc_redeem = signed_redeem(&btc_leg(), op(&bf), &party(1).btc);
        r.spenders.lock().unwrap().insert(bf, tx_hex(&btc_redeem));
        run_once(s, &matching(), &r);
        let sent = r.broadcasts();
        assert_eq!(sent.len(), 1);
        assert_eq!(decode(&sent[0]).output, signed.output);
    }

    #[test]
    fn waiting_redeem_ignores_a_btc_refund() {
        let t = temp_store("waiting-refund");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let xc = xbt_leg();
        let unsigned = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(2_000)).unwrap();
        let kind = WatchKind::WaitingRedeem {
            sigs_hex: vec![hex::encode(party(3).xbt.sign(&xc.spend_sighash(&unsigned, 0).unwrap()))],
            pubkey_hex: hex::encode(party(3).xbt.pubkey().serialize()),
            witness_script_hex: hex::encode(xc.witness_script.as_bytes()),
        };
        s.put_watch(&job(Chain::Xbt, kind, &xf, 0, vec![tx_hex(&unsigned)])).unwrap();
        let r = chain_at(NOW + 30 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xc, 8)));
        let refund = signed_refund(&btc_leg(), op(&bf), &party(3).btc, 1_000);
        r.spenders.lock().unwrap().insert(bf, tx_hex(&refund));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty());
    }

    #[test]
    fn redeem_broadcast_once_htlc_has_required_confs() {
        let t = temp_store("redeem");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let bc = btc_leg();
        let redeem = tx_hex(&signed_redeem(&bc, op(&bf), &party(1).btc));
        s.put_watch(&job(Chain::Btc, WatchKind::Redeem, &bf, 0, vec![redeem.clone()])).unwrap();

        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 8)));
        r.set_txout(Chain::Btc, &bf, Some(info(&bc, 1)));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty(), "btc_confs is 2");

        r.set_txout(Chain::Btc, &bf, Some(info(&bc, 2)));
        run_once(s, &matching(), &r);
        assert_eq!(r.broadcasts(), [redeem]);

        r.spend_confirms(Chain::Btc, &bf);
        run_once(s, &matching(), &r);
        assert!(only_job(s).done);
    }

    #[test]
    fn refund_rebroadcast_while_its_spend_is_unconfirmed() {
        let t = temp_store("refund-mempool");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        s.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
        let c = xbt_leg();
        let refund = tx_hex(&signed_refund(&c, op(&xf), &party(1).xbt, 1_000));
        let not_before = terms().xbt_locktime;
        s.put_watch(&job(Chain::Xbt, WatchKind::Refund, &xf, not_before, vec![refund.clone()])).unwrap();
        let r = chain_at(not_before as u64);
        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 10)));
        run_once(s, &matching(), &r);
        assert_eq!(r.broadcasts(), std::slice::from_ref(&refund));

        // Spent in the mempool only (our refund, which may yet be evicted):
        // not done, and the refund goes out again.
        r.set_txout(Chain::Xbt, &xf, None);
        run_once(s, &matching(), &r);
        assert!(!only_job(s).done, "a mempool spend is not final");
        assert_eq!(r.broadcasts(), [refund.clone(), refund.clone()]);

        r.spend_confirms(Chain::Xbt, &xf);
        run_once(s, &matching(), &r);
        assert!(only_job(s).done);
        assert_eq!(r.broadcasts().len(), 2, "nothing more once the spend confirms");
    }

    #[test]
    fn mempool_spend_of_a_job_never_sent_is_not_broadcast() {
        let t = temp_store("redeem-mempool");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let bc = btc_leg();
        let redeem = tx_hex(&signed_redeem(&bc, op(&bf), &party(1).btc));
        s.put_watch(&job(Chain::Btc, WatchKind::Redeem, &bf, 0, vec![redeem])).unwrap();
        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 8)));
        r.set_txout(Chain::Btc, &bf, Some(info(&bc, 1)));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty(), "btc_confs is 2");

        // Someone else's spend, in the mempool: nothing of ours to re-send.
        r.set_txout(Chain::Btc, &bf, None);
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty());
        assert!(!only_job(s).done);

        r.spend_confirms(Chain::Btc, &bf);
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty());
        assert!(only_job(s).done);
    }

    fn signed_redeem_fee(c: &HtlcContract, at: OutPoint, key: &SwapKey, fee: u64) -> Transaction {
        let mut tx = c.redeem_tx(at, ScriptBuf::new(), Amount::from_sat(fee)).unwrap();
        let sig = key.sign(&c.spend_sighash(&tx, 0).unwrap());
        c.finalize_redeem(&mut tx, 0, &sig, &key.pubkey(), &PREIMAGE);
        tx
    }

    /// [Final C1] The initiator's phone sent its BTC redeem itself, at a low
    /// rate. That mempool spend reveals this match's preimage, so it is ours:
    /// the job adopts it and re-sends the variant the current feerate needs,
    /// so a stalled redeem is fee-bumped before the participant can refund.
    #[test]
    fn phone_sent_btc_redeem_in_the_mempool_is_fee_bumped() {
        let t = temp_store("redeem-phone-sent");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let bc = btc_leg();
        let low = signed_redeem_fee(&bc, op(&bf), &party(1).btc, 1_000);
        let high = signed_redeem_fee(&bc, op(&bf), &party(1).btc, 50_000);
        s.put_watch(&job(Chain::Btc, WatchKind::Redeem, &bf, 0, vec![tx_hex(&low), tx_hex(&high)])).unwrap();

        // The phone's low-fee redeem sits in the mempool; the job never sent anything.
        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 8)));
        r.spenders.lock().unwrap().insert(bf.clone(), tx_hex(&low));
        run_once(s, &matching(), &r);
        assert_eq!(
            only_job(s).sent_prevout,
            Some((bc.value.to_sat(), hex::encode(bc.script_pubkey.as_bytes()))),
            "a mempool redeem revealing our preimage is ours"
        );

        // Fees rise: the dearer variant replaces it.
        *r.feerate.lock().unwrap() = 100;
        run_once(s, &matching(), &r);
        let sent: Vec<Transaction> = r.broadcasts().iter().map(|h| decode(h)).collect();
        assert_eq!(sent.last().unwrap().output, high.output, "re-sent at the current feerate");
        assert!(!only_job(s).done);

        r.spend_confirms(Chain::Btc, &bf);
        run_once(s, &matching(), &r);
        assert!(only_job(s).done);
    }

    /// [Final C1] A mempool spend that reveals no preimage (the participant's
    /// refund) is not ours: nothing is re-sent.
    #[test]
    fn mempool_refund_of_a_redeem_job_is_not_adopted() {
        let t = temp_store("redeem-mempool-refund");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let bc = btc_leg();
        let redeem = tx_hex(&signed_redeem(&bc, op(&bf), &party(1).btc));
        s.put_watch(&job(Chain::Btc, WatchKind::Redeem, &bf, 0, vec![redeem])).unwrap();
        let r = chain_at(NOW + 30 * 3600);
        let refund = signed_refund(&bc, op(&bf), &party(3).btc, 1_000);
        r.spenders.lock().unwrap().insert(bf, tx_hex(&refund));
        run_once(s, &matching(), &r);
        assert!(r.broadcasts().is_empty());
        assert_eq!(only_job(s).sent_prevout, None);
    }

    #[test]
    fn waiting_redeem_variants_each_carry_their_own_sig() {
        let t = temp_store("waiting-sigs");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let xc = xbt_leg();
        let part = party(3);
        let low = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(2_000)).unwrap();
        let high = xc.redeem_tx(op(&xf), ScriptBuf::new(), Amount::from_sat(50_000)).unwrap();
        let sig_of = |tx: &Transaction| hex::encode(part.xbt.sign(&xc.spend_sighash(tx, 0).unwrap()));
        let kind = WatchKind::WaitingRedeem {
            sigs_hex: vec![sig_of(&low), sig_of(&high)],
            pubkey_hex: hex::encode(part.xbt.pubkey().serialize()),
            witness_script_hex: hex::encode(xc.witness_script.as_bytes()),
        };
        s.put_watch(&job(Chain::Xbt, kind, &xf, 0, vec![tx_hex(&low), tx_hex(&high)])).unwrap();
        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xc, 8)));
        let btc_redeem = signed_redeem(&btc_leg(), op(&bf), &party(1).btc);
        r.spenders.lock().unwrap().insert(bf, tx_hex(&btc_redeem));

        // Fees are low: the cheap variant.
        run_once(s, &matching(), &r);
        // Fees rise while it sits in the mempool: the dearer one replaces it.
        r.set_txout(Chain::Xbt, &xf, None);
        *r.feerate.lock().unwrap() = 100;
        run_once(s, &matching(), &r);

        let sent: Vec<Transaction> = r.broadcasts().iter().map(|h| decode(h)).collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].output, low.output);
        assert_eq!(sent[1].output, high.output);
        let (_, xbt_params) = network_params("bitcoin").unwrap();
        let prevout = TxOut { value: xc.value, script_pubkey: xc.script_pubkey.clone() };
        for tx in &sent {
            assert_eq!(tx.input[0].witness.nth(2).unwrap(), &PREIMAGE[..]);
            assert!(signature_covers(tx, &prevout, xbt_params.sighash_variant()), "each variant is validly signed");
        }
        assert!(!only_job(s).done);
        r.spend_confirms(Chain::Xbt, &xf);
        run_once(s, &matching(), &r);
        assert!(only_job(s).done);
    }

    /// Serves `routes` (path-and-query → (status, body)); anything else 404s.
    fn stub(routes: Vec<(&'static str, u16, &'static str)>) -> Upstream {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| req.url() == *p)
                    .map(|(_, s, b)| (*s, *b))
                    .unwrap_or((404, "no route"));
                let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(status));
            }
        });
        Upstream::new(&url)
    }

    #[test]
    fn upstream_spend_confirmed_needs_a_confirmed_spend() {
        let x = "11".repeat(32);
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let xbt = stub(vec![
            // Spent in the mempool only: still in the confirmed UTXO set.
            (leak(format!("/txout/{x}/0?mempool=false")), 200, r#"{"value_sat":1,"scriptpubkey_hex":"00","confirmations":7}"#),
            (leak(format!("/txout/{x}/1?mempool=false")), 404, r#"{"error":"spent or unknown"}"#),
        ]);
        let btc = stub(vec![
            (leak(format!("/tx/{x}/outspend/0")), 200, r#"{"spent":true,"txid":"ab","status":{"confirmed":false}}"#),
            (leak(format!("/tx/{x}/outspend/1")), 200, r#"{"spent":true,"txid":"ab","status":{"confirmed":true,"block_height":5}}"#),
            (leak(format!("/tx/{x}/outspend/2")), 200, r#"{"spent":false}"#),
        ]);
        let r = UpstreamReader::new(Some(xbt), vec![btc], None);
        assert!(!r.spend_confirmed(Chain::Xbt, &x, 0).unwrap());
        assert!(r.spend_confirmed(Chain::Xbt, &x, 1).unwrap());
        assert!(r.spend_confirmed(Chain::Xbt, &x, 2).is_err(), "a bare 404 is not a spend");
        assert!(!r.spend_confirmed(Chain::Btc, &x, 0).unwrap());
        assert!(r.spend_confirmed(Chain::Btc, &x, 1).unwrap());
        assert!(!r.spend_confirmed(Chain::Btc, &x, 2).unwrap());
    }

    fn variant(prevout_value: u64, fee: u64) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[[0u8; 72].as_slice(), &[0u8; 33]]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(prevout_value - fee),
                script_pubkey: ScriptBuf::from(vec![0u8; 22]),
            }],
        }
    }

    #[test]
    fn pick_variant_lowest_meeting_feerate_else_highest() {
        let value = 100_000;
        let vsize = variant(value, 0).vsize() as u64;
        // Fees of 2, 5 and 10 sat/vB, in no particular order.
        let vs = [variant(value, 10 * vsize), variant(value, 2 * vsize), variant(value, 5 * vsize)];
        let fee = |tx: &Transaction| value - tx.output[0].value.to_sat();
        assert_eq!(fee(pick_variant(&vs, value, 1)), 2 * vsize);
        assert_eq!(fee(pick_variant(&vs, value, 2)), 2 * vsize);
        assert_eq!(fee(pick_variant(&vs, value, 3)), 5 * vsize);
        assert_eq!(fee(pick_variant(&vs, value, 6)), 10 * vsize);
        assert_eq!(fee(pick_variant(&vs, value, 50)), 10 * vsize, "none meets it: the highest");
    }

    #[test]
    fn participant_deadline_set_when_xbt_funding_reaches_6_confs() {
        let t = temp_store("deadline");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        s.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
        let c = xbt_leg();
        let now = NOW + 3600;
        let r = chain_at(now);

        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 5)));
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().participant_fund_by, None);
        assert!(s.msgs_after("m1", 0).is_empty());

        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 6)));
        run_once(s, &matching(), &r);
        let m = s.get_match("m1").unwrap();
        assert_eq!(m.participant_fund_by, Some((now + 7200) as u32));
        assert_eq!(m.status, MatchStatus::InitiatorFunded);
        let msgs = s.msgs_after("m1", 0);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].2["type"], "deadline");
        assert_eq!(msgs[0].2["participant_fund_by"], now + 7200);

        // Set once: a later tick neither moves it nor relays again.
        r.set_now(now + 600);
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().participant_fund_by, Some((now + 7200) as u32));
        assert_eq!(s.msgs_after("m1", 0).len(), 1);
    }

    #[test]
    fn participant_deadline_capped_12h_before_btc_locktime() {
        let t = temp_store("deadline-cap");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        s.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
        let now = NOW + 11 * 3600;
        let r = chain_at(now);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 6)));
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().participant_fund_by, Some(terms().btc_locktime - 12 * 3600));
    }

    /// A short BTC leg (regtest: 600 s) can't keep 12 h in hand; the
    /// participant keeps half of it instead.
    #[test]
    fn participant_deadline_leaves_half_of_a_short_btc_leg() {
        let t = temp_store("deadline-short");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        let mut m = a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None);
        m.terms.btc_locktime = m.terms.matched_at + 600;
        s.put_match(&m).unwrap();
        let r = chain_at(NOW + 60);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 6)));
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().participant_fund_by, Some((NOW + 300) as u32));
        // Still the participant's turn on the next tick.
        r.set_now(NOW + 61);
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().status, MatchStatus::InitiatorFunded);
    }

    #[test]
    fn spawn_runs_a_pass_every_tick() {
        let dir = crate::test_tmp::TempDir::new("watchtower-test-spawn");
        let store = Arc::new(Store::open(&dir).unwrap());
        let xf = outpoint(0xaa, 0);
        store.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
        let c = xbt_leg();
        let refund = tx_hex(&signed_refund(&c, op(&xf), &party(1).xbt, 1_000));
        store.put_watch(&job(Chain::Xbt, WatchKind::Refund, &xf, 0, vec![refund])).unwrap();
        let r = Arc::new(chain_at(NOW));
        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 6)));
        spawn(store, matching(), r.clone(), Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(400));
        let sent = r.broadcasts().len();
        assert!(sent >= 3, "a 50 ms tick resends the unspent refund every pass: {sent}");
    }

    #[test]
    fn upstream_reader_now_includes_the_clock_offset() {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let r = UpstreamReader::new(None, vec![], None).with_clock_offset(5_000);
        let now = r.now();
        assert!((wall + 5_000..=wall + 5_002).contains(&now), "{now} vs {wall}");
        assert!(UpstreamReader::new(None, vec![], None).now() <= wall + 2);
    }

    #[test]
    fn participant_deadline_not_started_by_a_wrong_funding_output() {
        let c = xbt_leg();
        let short = TxOutInfo { value_sat: c.value.to_sat() - 1, ..info(&c, 6) };
        let over = TxOutInfo { value_sat: c.value.to_sat() + 1, ..info(&c, 6) };
        // The BTC leg's script: a real HTLC, but not this one.
        let wrong_script = TxOutInfo {
            scriptpubkey_hex: hex::encode(btc_leg().script_pubkey.as_bytes()),
            ..info(&c, 6)
        };
        for (label, bad) in [("short", short), ("over", over), ("script", wrong_script)] {
            let t = temp_store(label);
            let s = &t.store;
            let xf = outpoint(0xaa, 0);
            s.put_match(&a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None)).unwrap();
            let r = chain_at(NOW + 3600);
            r.set_txout(Chain::Xbt, &xf, Some(bad));
            run_once(s, &matching(), &r);
            assert_eq!(s.get_match("m1").unwrap().participant_fund_by, None, "{label}");
            assert!(s.msgs_after("m1", 0).is_empty(), "{label}");
        }

        // Without the relayed keys there is no expected script: no clock either.
        let t = temp_store("nokeys");
        let s = &t.store;
        let xf = outpoint(0xaa, 0);
        let mut m = a_match(MatchStatus::InitiatorFunded, Some(xf.clone()), None);
        m.keys.1 = None;
        s.put_match(&m).unwrap();
        let r = chain_at(NOW + 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&c, 6)));
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().participant_fund_by, None);
    }

    #[test]
    fn match_closed_when_both_htlcs_spent() {
        let t = temp_store("closed");
        let s = &t.store;
        let (xf, bf) = (outpoint(0xaa, 0), outpoint(0xbb, 1));
        s.put_match(&a_match(MatchStatus::ParticipantFunded, Some(xf.clone()), Some(bf.clone()))).unwrap();
        let r = chain_at(NOW + 3 * 3600);
        r.set_txout(Chain::Xbt, &xf, Some(info(&xbt_leg(), 8)));
        r.set_txout(Chain::Btc, &bf, Some(info(&btc_leg(), 3)));
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().status, MatchStatus::ParticipantFunded);

        r.set_txout(Chain::Btc, &bf, None);
        run_once(s, &matching(), &r);
        assert_eq!(s.get_match("m1").unwrap().status, MatchStatus::ParticipantFunded, "one leg left");

        r.set_txout(Chain::Xbt, &xf, None);
        run_once(s, &matching(), &r);
        let m = s.get_match("m1").unwrap();
        assert_eq!(m.status, MatchStatus::Closed);
        assert_eq!(m.closed_at, Some(NOW + 3 * 3600));
    }
}
