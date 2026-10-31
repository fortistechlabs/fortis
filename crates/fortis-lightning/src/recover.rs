//! Seed-only recovery of force-closed channel balances.
//!
//! When all channel state is lost (VSS data gone) and the LSP force-closes,
//! our settled balance lands on one of LDK 0.2's static, seed-derived
//! scripts (`KeysManager::possible_v2_counterparty_closed_balance_spks`,
//! on in ldk-node 0.7.1 via `v2_remote_key_derivation`). ldk-node does not
//! scan for or sweep those outputs itself (spec, *Findings (Task 1) (a)*);
//! this module does, from the 64-byte node seed alone, with no running
//! node:
//!
//! 1. [`static_scripts`] derives the 2000 candidate scripts,
//! 2. [`scan_static_outputs`] asks a [`ScriptUtxoSource`] ([`BitcoindScan`]
//!    or [`EsploraScan`]) which of them hold UTXOs,
//! 3. [`sweep_static_outputs`] spends them in one transaction to an address
//!    and hands it to a [`Broadcaster`] ([`BitcoindBroadcast`] or
//!    [`EsploraBroadcast`]).
//!
//! **Derivation** (lightning 0.2.7 `sign/mod.rs`, ldk-node 0.7.1
//! `builder.rs`): ldk-node seeds its `KeysManager` with the BIP-32 master
//! private key of the 64 seed bytes; `KeysManager::new` takes a second
//! BIP-32 master of those 32 bytes and derives `static_payment_key =
//! m/8'`. Key `k` (`k < STATIC_PAYMENT_KEY_COUNT` = 1000) is `m/8'/k'`. For
//! each key LDK lists two scripts, in this order: P2WPKH(pubkey)
//! (static_remote_key channels) and P2WSH(`<pubkey> OP_CHECKSIGVERIFY 1
//! OP_CHECKSEQUENCEVERIFY`) (anchor channels). Script index `i` in
//! `0..2000` is therefore key `i / 2`, P2WPKH when `i` is even and P2WSH
//! when odd. None of it depends on `KeysManager`'s starting time (that only
//! seeds its RNG) or on the network.
//!
//! No secret ever appears in an error, a log line or a `Debug` string;
//! derived private keys are erased when dropped (best effort: secp256k1
//! secret keys are `Copy`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitcoin::absolute::LockTime;
use bitcoin::base64::Engine as _;
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{self, Message, PublicKey, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{
    ecdsa, Address, Amount, CompressedPublicKey, Network, OutPoint, ScriptBuf, Sequence,
    Transaction, TxIn, TxOut, Txid, Witness,
};
use ldk_node::lightning::ln::chan_utils::get_to_countersigner_keyed_anchor_redeemscript;
use ldk_node::lightning::sign::STATIC_PAYMENT_KEY_COUNT;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::error::LnError;
use crate::lnurl::{read_capped, MAX_BODY};

/// Number of static scripts: two per static payment key.
pub const STATIC_SCRIPT_COUNT: u32 = 2 * STATIC_PAYMENT_KEY_COUNT as u32;
/// Smallest output the sweep will create (the P2PKH dust limit, which
/// covers every standard destination type).
pub const DUST_LIMIT_SAT: u64 = 546;
/// Highest accepted sweep feerate.
pub const MAX_FEERATE_SAT_PER_VB: u64 = 10_000;
/// 21 million BTC.
const MAX_MONEY_SAT: u64 = 21_000_000 * 100_000_000;
/// Largest standard transaction weight (Bitcoin Core `MAX_STANDARD_TX_WEIGHT`).
const MAX_STANDARD_WEIGHT: u64 = 400_000;
/// LDK's static-payment-key branch, `m/8'`.
const STATIC_PAYMENT_KEY_INDEX: u32 = 8;

/// A UTXO paying one of our static scripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticUtxo {
    pub outpoint: OutPoint,
    pub value_sat: u64,
    /// Index into [`static_scripts`] (`0..STATIC_SCRIPT_COUNT`).
    pub script_index: u32,
    /// 0 = unconfirmed.
    pub confirmations: u32,
}

/// A UTXO as reported by a [`ScriptUtxoSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtxoRef {
    pub outpoint: OutPoint,
    pub value_sat: u64,
    pub confirmations: u32,
}

/// Finds the UTXOs paying a batch of scripts.
pub trait ScriptUtxoSource {
    /// How many scripts [`scan_static_outputs`] passes per call (progress is
    /// reported after each call).
    fn batch_size(&self) -> usize {
        100
    }
    /// UTXOs paying `scripts`, each tagged with the index into `scripts` of
    /// the script it pays.
    fn utxos_for_scripts(&self, scripts: &[ScriptBuf]) -> Result<Vec<(usize, UtxoRef)>, LnError>;
}

/// Publishes a transaction.
pub trait Broadcaster {
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, LnError>;
}

/// The static scripts for `node_seed` (the 64-byte BIP-85 node seed handed
/// to ldk-node), as `(script_index, script_pubkey)`, in LDK's order. Always
/// [`STATIC_SCRIPT_COUNT`] entries (empty only if BIP-32 derivation fails,
/// which happens with negligible probability). `network` does not change
/// the scripts.
pub fn static_scripts(node_seed: &[u8; 64], network: Network) -> Vec<(u32, ScriptBuf)> {
    let _ = network;
    try_static_scripts(node_seed).unwrap_or_default()
}

fn derivation_failed() -> LnError {
    LnError::Other("key derivation failed".into())
}

/// A secret key erased on drop.
struct Sk(SecretKey);

impl Drop for Sk {
    fn drop(&mut self) {
        self.0.non_secure_erase();
    }
}

/// LDK's `static_payment_key` (`m/8'` of the KeysManager seed), erased on
/// drop. Deliberately not `Debug`.
struct StaticKeys {
    base: Xpriv,
    secp: Secp256k1<secp256k1::All>,
}

impl Drop for StaticKeys {
    fn drop(&mut self) {
        self.base.private_key.non_secure_erase();
        self.base.chain_code = bitcoin::bip32::ChainCode::from([0u8; 32]);
    }
}

impl StaticKeys {
    fn new(node_seed: &[u8; 64]) -> Result<Self, LnError> {
        // ldk-node 0.7.1 builder.rs: KeysManager seed = BIP-32 master
        // private key of the 64 bytes (the network only affects
        // serialisation, not the key).
        let mut master =
            Xpriv::new_master(Network::Testnet, node_seed).map_err(|_| derivation_failed())?;
        let km_seed = Zeroizing::new(master.private_key.secret_bytes());
        master.private_key.non_secure_erase();
        // lightning 0.2.7 KeysManager::new: master of the 32-byte seed
        // (Network::Testnet there), static_payment_key = m/8'.
        let mut km_master =
            Xpriv::new_master(Network::Testnet, &km_seed[..]).map_err(|_| derivation_failed())?;
        let secp = Secp256k1::new();
        let base = km_master.derive_priv(
            &secp,
            &[ChildNumber::Hardened {
                index: STATIC_PAYMENT_KEY_INDEX,
            }],
        );
        km_master.private_key.non_secure_erase();
        Ok(Self {
            base: base.map_err(|_| derivation_failed())?,
            secp,
        })
    }

    /// Key `k` = `m/8'/k'`.
    fn key(&self, k: u32) -> Result<Sk, LnError> {
        if k >= u32::from(STATIC_PAYMENT_KEY_COUNT) {
            return Err(derivation_failed());
        }
        let mut x = self
            .base
            .derive_priv(&self.secp, &[ChildNumber::Hardened { index: k }])
            .map_err(|_| derivation_failed())?;
        let sk = Sk(x.private_key);
        x.private_key.non_secure_erase();
        Ok(sk)
    }
}

/// What a static script index stands for.
enum Template {
    /// static_remote_key channels: P2WPKH(pubkey).
    P2wpkh,
    /// Anchor channels: P2WSH(witness_script).
    Anchor(ScriptBuf),
}

/// Script `i`: key `i / 2`; even = P2WPKH, odd = anchor P2WSH (LDK's order).
fn template(i: u32, pk: &PublicKey) -> (ScriptBuf, Template) {
    if i.is_multiple_of(2) {
        (
            ScriptBuf::new_p2wpkh(&CompressedPublicKey(*pk).wpubkey_hash()),
            Template::P2wpkh,
        )
    } else {
        let ws = get_to_countersigner_keyed_anchor_redeemscript(pk);
        (ws.to_p2wsh(), Template::Anchor(ws))
    }
}

fn try_static_scripts(node_seed: &[u8; 64]) -> Result<Vec<(u32, ScriptBuf)>, LnError> {
    let keys = StaticKeys::new(node_seed)?;
    let mut out = Vec::with_capacity(STATIC_SCRIPT_COUNT as usize);
    for k in 0..u32::from(STATIC_PAYMENT_KEY_COUNT) {
        let sk = keys.key(k)?;
        let pk = PublicKey::from_secret_key(&keys.secp, &sk.0);
        for i in [2 * k, 2 * k + 1] {
            out.push((i, template(i, &pk).0));
        }
    }
    Ok(out)
}

/// Scans every static script of `node_seed` with `source`, batch by batch,
/// calling `progress(done, total)` before the first batch and after each.
/// Returns the UTXOs found, sorted by outpoint, de-duplicated; zero-value
/// outputs are dropped. A source reporting an out-of-range index, a value
/// above 21M BTC or one outpoint with two different scripts/values fails
/// the scan (`scan_failed: …`).
pub fn scan_static_outputs(
    node_seed: &[u8; 64],
    network: Network,
    source: &dyn ScriptUtxoSource,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<Vec<StaticUtxo>, LnError> {
    let _ = network;
    let scripts: Vec<ScriptBuf> = try_static_scripts(node_seed)?
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    let total = scripts.len() as u32;
    let batch = source.batch_size().clamp(1, scripts.len().max(1));
    let fail = |m: &str| LnError::Other(format!("scan_failed: {m}"));
    let mut found: HashMap<OutPoint, StaticUtxo> = HashMap::new();
    progress(0, total);
    let mut done = 0usize;
    for chunk in scripts.chunks(batch) {
        for (i, u) in source.utxos_for_scripts(chunk)? {
            if i >= chunk.len() {
                return Err(fail("source returned an out-of-range script index"));
            }
            if u.value_sat > MAX_MONEY_SAT {
                return Err(fail("source returned an out-of-range value"));
            }
            if u.value_sat == 0 {
                continue;
            }
            let utxo = StaticUtxo {
                outpoint: u.outpoint,
                value_sat: u.value_sat,
                script_index: (done + i) as u32,
                confirmations: u.confirmations,
            };
            match found.get_mut(&u.outpoint) {
                None => {
                    found.insert(u.outpoint, utxo);
                }
                Some(prev)
                    if prev.script_index == utxo.script_index
                        && prev.value_sat == utxo.value_sat =>
                {
                    prev.confirmations = prev.confirmations.max(utxo.confirmations);
                }
                Some(_) => return Err(fail("source returned conflicting data for one outpoint")),
            }
        }
        done += chunk.len();
        progress(done as u32, total);
    }
    let mut out: Vec<StaticUtxo> = found.into_values().collect();
    out.sort_by_key(|u| u.outpoint);
    Ok(out)
}

/// Spends all confirmed `utxos` in one transaction paying `to_address`
/// (minus the fee) and broadcasts it. Unconfirmed UTXOs are skipped (the
/// anchor script needs one confirmation). Version 2, locktime 0, inputs
/// sorted by outpoint, each with nSequence 1, one output; fee =
/// vsize(signed tx) × `feerate_sat_per_vb` (vsize = weight / 4 rounded up).
///
/// `Ok(None)`: nothing to spend, or the output would be below
/// [`DUST_LIMIT_SAT`]. Errors (never after a partial broadcast): feerate 0
/// or above [`MAX_FEERATE_SAT_PER_VB`] (`invalid_feerate`), `to_address`
/// not for `network` (`WrongNetwork`), an invalid UTXO (bad script index,
/// zero or absurd value, duplicate outpoint), a transaction that does not
/// fully verify or is non-standard by weight, and broadcast failures
/// (`broadcast_failed: …`).
pub fn sweep_static_outputs(
    node_seed: &[u8; 64],
    network: Network,
    utxos: &[StaticUtxo],
    to_address: &Address,
    feerate_sat_per_vb: u64,
    broadcaster: &dyn Broadcaster,
) -> Result<Option<Txid>, LnError> {
    if feerate_sat_per_vb == 0 || feerate_sat_per_vb > MAX_FEERATE_SAT_PER_VB {
        return Err(LnError::Other("invalid_feerate".into()));
    }
    if !to_address.as_unchecked().is_valid_for_network(network) {
        return Err(LnError::WrongNetwork);
    }
    let invalid = |m: &str| LnError::Other(format!("sweep: {m}"));
    let mut seen = HashSet::new();
    for u in utxos {
        if u.script_index >= STATIC_SCRIPT_COUNT {
            return Err(invalid("invalid script index"));
        }
        if u.value_sat == 0 || u.value_sat > MAX_MONEY_SAT {
            return Err(invalid("invalid value"));
        }
        if !seen.insert(u.outpoint) {
            return Err(invalid("duplicate outpoint"));
        }
    }
    let mut spend: Vec<&StaticUtxo> = utxos.iter().filter(|u| u.confirmations >= 1).collect();
    if spend.is_empty() {
        return Ok(None);
    }
    spend.sort_by_key(|u| u.outpoint);
    let total = spend
        .iter()
        .try_fold(0u64, |a, u| a.checked_add(u.value_sat))
        .filter(|t| *t <= MAX_MONEY_SAT)
        .ok_or_else(|| invalid("invalid value"))?;

    // Keys and prevouts, in input order.
    let keys = StaticKeys::new(node_seed)?;
    let mut signers = Vec::with_capacity(spend.len());
    let mut prevouts = Vec::with_capacity(spend.len());
    for u in &spend {
        let sk = keys.key(u.script_index / 2)?;
        let pk = PublicKey::from_secret_key(&keys.secp, &sk.0);
        let (spk, tpl) = template(u.script_index, &pk);
        prevouts.push(TxOut {
            value: Amount::from_sat(u.value_sat),
            script_pubkey: spk,
        });
        signers.push((sk, pk, tpl));
    }

    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: spend
            .iter()
            .map(|u| TxIn {
                previous_output: u.outpoint,
                script_sig: ScriptBuf::new(),
                // The anchor script's 1-block CSV (harmless for P2WPKH).
                sequence: Sequence(1),
                witness: Witness::new(),
            })
            .collect(),
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: to_address.script_pubkey(),
        }],
    };

    // fee = vsize(signed) × feerate. The output value does not change the
    // size, but re-signing can (rarely) change a signature's length, so
    // iterate to the fixed point; if it does not settle, keep a fee that
    // covers the last signed size.
    let mut fee = 0u64;
    let mut settled = false;
    for _ in 0..8 {
        let Some(net) = total.checked_sub(fee).filter(|n| *n >= DUST_LIMIT_SAT) else {
            return Ok(None);
        };
        tx.output[0].value = Amount::from_sat(net);
        sign(&mut tx, &prevouts, &signers, &keys.secp)?;
        let required = tx
            .weight()
            .to_wu()
            .div_ceil(4)
            .checked_mul(feerate_sat_per_vb)
            .ok_or_else(|| invalid("fee overflow"))?;
        if required == fee {
            settled = true;
            break;
        }
        if required < fee && fee - required <= 4 * feerate_sat_per_vb {
            settled = true; // a signature shrank: overpay by < 4 vB.
            break;
        }
        fee = required;
    }
    if !settled {
        return Err(invalid("fee did not settle"));
    }
    if total.checked_sub(fee).is_none_or(|n| n < DUST_LIMIT_SAT) {
        return Ok(None);
    }
    if tx.weight().to_wu() > MAX_STANDARD_WEIGHT {
        return Err(invalid("too many inputs for one transaction"));
    }
    verify_spend(&tx, &prevouts).map_err(|_| invalid("signing failed"))?;
    drop(signers);

    let txid = tx.compute_txid();
    let sent = broadcaster.broadcast(&tx).map_err(|e| match e {
        LnError::Other(m) if m.starts_with("broadcast_failed:") => LnError::Other(m),
        LnError::Other(m) => LnError::Other(format!("broadcast_failed: {m}")),
        e => LnError::Other(format!("broadcast_failed: {e}")),
    })?;
    if sent != txid {
        return Err(LnError::Other("broadcast_failed: txid mismatch".into()));
    }
    Ok(Some(txid))
}

/// Signs every input (SIGHASH_ALL, low-R) with its static key.
fn sign(
    tx: &mut Transaction,
    prevouts: &[TxOut],
    signers: &[(Sk, PublicKey, Template)],
    secp: &Secp256k1<secp256k1::All>,
) -> Result<(), LnError> {
    let fail = || LnError::Other("sweep: signing failed".into());
    let mut witnesses = Vec::with_capacity(signers.len());
    {
        let mut cache = SighashCache::new(&*tx);
        for (i, ((sk, pk, tpl), prev)) in signers.iter().zip(prevouts).enumerate() {
            let sighash = match tpl {
                Template::Anchor(ws) => cache
                    .p2wsh_signature_hash(i, ws, prev.value, EcdsaSighashType::All)
                    .map_err(|_| fail())?,
                Template::P2wpkh => cache
                    .p2wpkh_signature_hash(
                        i,
                        &prev.script_pubkey,
                        prev.value,
                        EcdsaSighashType::All,
                    )
                    .map_err(|_| fail())?,
            };
            let msg = Message::from_digest(sighash.to_byte_array());
            let sig = ecdsa::Signature {
                signature: secp.sign_ecdsa_low_r(&msg, &sk.0),
                sighash_type: EcdsaSighashType::All,
            };
            witnesses.push(match tpl {
                Template::Anchor(ws) => Witness::from_slice(&[sig.to_vec(), ws.to_bytes()]),
                Template::P2wpkh => Witness::from_slice(&[sig.to_vec(), pk.serialize().to_vec()]),
            });
        }
    }
    for (input, w) in tx.input.iter_mut().zip(witnesses) {
        input.witness = w;
    }
    Ok(())
}

/// Esplora's script hash: sha256(script), byte-reversed, hex.
pub fn esplora_scripthash(script: &ScriptBuf) -> String {
    let mut h = sha256::Hash::hash(script.as_bytes()).to_byte_array();
    h.reverse();
    hex::encode(h)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// An HTTP response: status and (capped) body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The small HTTP client Esplora access goes through (so it can be faked).
/// Unlike [`crate::lnurl::HttpGet`] it exposes the status code, which the
/// scan needs to retry 429/5xx. `Err` = transport failure (no response).
pub trait HttpClient: Send + Sync {
    fn get(&self, url: &str) -> Result<HttpResponse, String>;
    fn post(&self, url: &str, content_type: &str, body: &str) -> Result<HttpResponse, String>;
}

/// Production blocking client: rustls, 10 s per-request timeout, no
/// redirects, body capped at the LNURL client's limit (64 KiB).
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .redirects(0)
            .build();
        Self { agent }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

fn ureq_response(
    r: Result<ureq::Response, ureq::Error>,
    cap: usize,
) -> Result<HttpResponse, String> {
    let resp = match r {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(ureq::Error::Transport(t)) => return Err(format!("transport: {}", t.kind())),
    };
    let status = resp.status();
    let body = read_capped(resp.into_reader(), cap).map_err(|e| e.to_string())?;
    Ok(HttpResponse { status, body })
}

impl HttpClient for UreqHttp {
    fn get(&self, url: &str) -> Result<HttpResponse, String> {
        ureq_response(self.agent.get(url).call(), 4 * 1024 * 1024)
    }
    fn post(&self, url: &str, content_type: &str, body: &str) -> Result<HttpResponse, String> {
        ureq_response(
            self.agent
                .post(url)
                .set("Content-Type", content_type)
                .send_string(body),
            MAX_BODY,
        )
    }
}

// ---------------------------------------------------------------------------
// Esplora
// ---------------------------------------------------------------------------

/// Esplora / mempool.space REST source: `GET /blocks/tip/height` once per
/// batch, then `GET /scripthash/:hash/utxo` per script.
///
/// Politeness: [`EsploraScan::BATCH`] (40) scripts per batch, at most
/// [`EsploraScan::CONCURRENCY`] (8) requests in flight, a pause (default
/// 250 ms) between batches, a 10 s per-request timeout, and up to 3 retries
/// with exponential backoff (500 ms, 1 s, 2 s) on 429, 5xx and transport
/// errors. A full scan is 2000 lookups in 50 batches. Any request that
/// still fails, any other non-2xx status (404 on a script = no UTXOs) or a
/// malformed body aborts the scan with `scan_failed: …`.
pub struct EsploraScan {
    base: String,
    http: Arc<dyn HttpClient>,
    retries: u32,
    backoff: Duration,
    pause: Duration,
    batches: AtomicUsize,
}

impl EsploraScan {
    pub const BATCH: usize = 40;
    pub const CONCURRENCY: usize = 8;

    /// `base_url`: the API root, e.g. `https://mempool.space/api`.
    pub fn new(base_url: &str) -> Self {
        Self::with_client(base_url, Arc::new(UreqHttp::new()))
    }

    pub fn with_client(base_url: &str, http: Arc<dyn HttpClient>) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
            retries: 3,
            backoff: Duration::from_millis(500),
            pause: Duration::from_millis(250),
            batches: AtomicUsize::new(0),
        }
    }

    /// First retry delay (doubles per retry).
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Delay before each batch after the first.
    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }
}

impl ScriptUtxoSource for EsploraScan {
    fn batch_size(&self) -> usize {
        Self::BATCH
    }

    fn utxos_for_scripts(&self, scripts: &[ScriptBuf]) -> Result<Vec<(usize, UtxoRef)>, LnError> {
        let fail = |m: String| LnError::Other(format!("scan_failed: {m}"));
        // Pace consecutive batches (the first call of a scan is immediate).
        if self.batches.fetch_add(1, Ordering::SeqCst) > 0 {
            std::thread::sleep(self.pause);
        }
        let tip_body = self
            .get(&format!("{}/blocks/tip/height", self.base))?
            .ok_or_else(|| fail("tip height: http 404".into()))?;
        let tip: u64 = std::str::from_utf8(&tip_body)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| fail("tip height: malformed response".into()))?;

        let next = AtomicUsize::new(0);
        let abort = AtomicBool::new(false);
        let results: Mutex<Vec<(usize, UtxoRef)>> = Mutex::new(Vec::new());
        let first_err: Mutex<Option<LnError>> = Mutex::new(None);
        let workers = Self::CONCURRENCY.min(scripts.len());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    if abort.load(Ordering::SeqCst) {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(script) = scripts.get(i) else { return };
                    let url = format!(
                        "{}/scripthash/{}/utxo",
                        self.base,
                        esplora_scripthash(script)
                    );
                    let r = self.get(&url).and_then(|body| match body {
                        None => Ok(vec![]),
                        Some(b) => parse_esplora_utxos(&b, tip),
                    });
                    match r {
                        Ok(us) => {
                            let mut res = results.lock().unwrap_or_else(|p| p.into_inner());
                            res.extend(us.into_iter().map(|u| (i, u)));
                        }
                        Err(e) => {
                            abort.store(true, Ordering::SeqCst);
                            first_err
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .get_or_insert(e);
                            return;
                        }
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap_or_else(|p| p.into_inner()) {
            return Err(e);
        }
        let mut out = results.into_inner().unwrap_or_else(|p| p.into_inner());
        out.sort_by_key(|(i, u)| (*i, u.outpoint));
        Ok(out)
    }
}

impl EsploraScan {
    /// GET with bounded retry. `Ok(None)` = 404.
    fn get(&self, url: &str) -> Result<Option<Vec<u8>>, LnError> {
        let mut attempt = 0u32;
        loop {
            let why = match self.http.get(url) {
                Ok(r) if (200..300).contains(&r.status) => return Ok(Some(r.body)),
                Ok(r) if r.status == 404 => return Ok(None),
                Ok(r) if r.status == 429 || r.status >= 500 => format!("http {}", r.status),
                Ok(r) => return Err(LnError::Other(format!("scan_failed: http {}", r.status))),
                Err(e) => e,
            };
            if attempt >= self.retries {
                return Err(LnError::Other(format!(
                    "scan_failed: {why} after {} attempts",
                    attempt + 1
                )));
            }
            std::thread::sleep(self.backoff.saturating_mul(1 << attempt));
            attempt += 1;
        }
    }
}

/// Parses an Esplora `/scripthash/:hash/utxo` body.
fn parse_esplora_utxos(body: &[u8], tip: u64) -> Result<Vec<UtxoRef>, LnError> {
    let bad = || LnError::Other("scan_failed: malformed response".into());
    let v: Value = serde_json::from_slice(body).map_err(|_| bad())?;
    let arr = v.as_array().ok_or_else(bad)?;
    arr.iter()
        .map(|u| {
            let txid: Txid = u
                .get("txid")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .ok_or_else(bad)?;
            let vout = u
                .get("vout")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(bad)?;
            let value_sat = u.get("value").and_then(Value::as_u64).ok_or_else(bad)?;
            let status = u.get("status").ok_or_else(bad)?;
            let confirmed = status
                .get("confirmed")
                .and_then(Value::as_bool)
                .ok_or_else(bad)?;
            let confirmations = if confirmed {
                let h = status
                    .get("block_height")
                    .and_then(Value::as_u64)
                    .ok_or_else(bad)?;
                confirmations(tip, h)
            } else {
                0
            };
            Ok(UtxoRef {
                outpoint: OutPoint { txid, vout },
                value_sat,
                confirmations,
            })
        })
        .collect()
}

/// Confirmations of a tx mined at `height` with the chain at `tip`; at
/// least 1 (a source's tip may lag its UTXO index).
fn confirmations(tip: u64, height: u64) -> u32 {
    u32::try_from(tip.saturating_sub(height).saturating_add(1)).unwrap_or(u32::MAX)
}

/// Esplora `POST /tx` broadcaster.
pub struct EsploraBroadcast {
    base: String,
    http: Arc<dyn HttpClient>,
}

impl EsploraBroadcast {
    pub fn new(base_url: &str) -> Self {
        Self::with_client(base_url, Arc::new(UreqHttp::new()))
    }
    pub fn with_client(base_url: &str, http: Arc<dyn HttpClient>) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
        }
    }
}

impl Broadcaster for EsploraBroadcast {
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, LnError> {
        let fail = |m: String| LnError::Other(format!("broadcast_failed: {m}"));
        let r = self
            .http
            .post(
                &format!("{}/tx", self.base),
                "text/plain",
                &serialize_hex(tx),
            )
            .map_err(fail)?;
        let text = String::from_utf8_lossy(&r.body);
        if !(200..300).contains(&r.status) {
            let reason: String = text
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            return Err(fail(format!("http {}: {reason}", r.status)));
        }
        returned_txid(text.trim(), tx)
    }
}

/// Checks a broadcaster's answer is `tx`'s own txid.
fn returned_txid(s: &str, tx: &Transaction) -> Result<Txid, LnError> {
    let got: Txid = s
        .parse()
        .map_err(|_| LnError::Other("broadcast_failed: malformed txid in response".into()))?;
    if got != tx.compute_txid() {
        return Err(LnError::Other("broadcast_failed: txid mismatch".into()));
    }
    Ok(got)
}

// ---------------------------------------------------------------------------
// bitcoind
// ---------------------------------------------------------------------------

/// Minimal bitcoind JSON-RPC client (HTTP basic auth).
struct Rpc {
    agent: ureq::Agent,
    url: String,
    auth: Zeroizing<String>,
}

impl Rpc {
    fn new(url: &str, user: &str, pass: &str, timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build();
        let creds = Zeroizing::new(format!("{user}:{pass}"));
        let auth = Zeroizing::new(format!(
            "Basic {}",
            bitcoin::base64::engine::general_purpose::STANDARD.encode(creds.as_bytes())
        ));
        Self {
            agent,
            url: url.to_string(),
            auth,
        }
    }

    /// Calls `method`; `Err` carries the transport error or bitcoind's
    /// error message.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let req = serde_json::json!({"jsonrpc": "1.0", "id": "fortis", "method": method, "params": params});
        let r = self
            .agent
            .post(&self.url)
            .set("Authorization", self.auth.as_str())
            .set("Content-Type", "application/json")
            .send_string(&req.to_string());
        let resp = ureq_response(r, 4 * 1024 * 1024)?;
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|_| format!("rpc: http {} with a non-json body", resp.status))?;
        if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
            let msg = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!(
                "rpc: {}",
                msg.chars().take(200).collect::<String>()
            ));
        }
        if !(200..300).contains(&resp.status) {
            return Err(format!("rpc: http {}", resp.status));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| "rpc: no result".to_string())
    }
}

/// bitcoind `scantxoutset` source (no wallet needed): one call with
/// `raw(<script hex>)` descriptors for all 2000 scripts. Scans the
/// confirmed UTXO set only. Timeout 10 min (a mainnet UTXO-set scan takes
/// minutes; regtest is instant).
pub struct BitcoindScan {
    rpc: Rpc,
}

impl BitcoindScan {
    /// `url`: e.g. `http://127.0.0.1:8332`.
    pub fn new(url: &str, user: &str, pass: &str) -> Self {
        Self {
            rpc: Rpc::new(url, user, pass, Duration::from_secs(600)),
        }
    }
}

impl ScriptUtxoSource for BitcoindScan {
    fn batch_size(&self) -> usize {
        STATIC_SCRIPT_COUNT as usize
    }

    fn utxos_for_scripts(&self, scripts: &[ScriptBuf]) -> Result<Vec<(usize, UtxoRef)>, LnError> {
        let descs: Vec<Value> = scripts
            .iter()
            .map(|s| Value::String(format!("raw({})", s.to_hex_string())))
            .collect();
        let res = self
            .rpc
            .call("scantxoutset", serde_json::json!(["start", descs]))
            .map_err(|e| LnError::Other(format!("scan_failed: {e}")))?;
        parse_scantxoutset(&res, scripts)
    }
}

/// Maps a `scantxoutset start` result back to indices into `scripts`.
fn parse_scantxoutset(
    res: &Value,
    scripts: &[ScriptBuf],
) -> Result<Vec<(usize, UtxoRef)>, LnError> {
    let bad = |m: &str| LnError::Other(format!("scan_failed: {m}"));
    if res.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(bad("scantxoutset did not complete"));
    }
    let tip = res
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("malformed response"))?;
    let index: HashMap<&[u8], usize> = scripts
        .iter()
        .enumerate()
        .rev()
        .map(|(i, s)| (s.as_bytes(), i))
        .collect();
    let unspents = res
        .get("unspents")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("malformed response"))?;
    unspents
        .iter()
        .map(|u| {
            let m = || bad("malformed response");
            let txid: Txid = u
                .get("txid")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .ok_or_else(m)?;
            let vout = u
                .get("vout")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(m)?;
            let spk = u
                .get("scriptPubKey")
                .and_then(Value::as_str)
                .and_then(|h| hex::decode(h).ok())
                .ok_or_else(m)?;
            let i = *index
                .get(spk.as_slice())
                .ok_or_else(|| bad("unexpected script in result"))?;
            let btc = u.get("amount").and_then(Value::as_f64).ok_or_else(m)?;
            let sat = btc * 1e8;
            if !sat.is_finite() || !(0.0..=MAX_MONEY_SAT as f64).contains(&sat.round()) {
                return Err(bad("amount out of range"));
            }
            let height = u.get("height").and_then(Value::as_u64).ok_or_else(m)?;
            let utxo = UtxoRef {
                outpoint: OutPoint { txid, vout },
                value_sat: sat.round() as u64,
                confirmations: confirmations(tip, height),
            };
            Ok((i, utxo))
        })
        .collect()
}

/// Independent check of a sweep: every input executes its prevout's
/// template (P2WSH anchor or P2WPKH) with a valid SIGHASH_ALL signature.
///
/// Not a general script interpreter: it accepts exactly the two templates
/// and checks what executing them checks — the witness script / key hashes
/// to the prevout, the signature is DER + SIGHASH_ALL and valid for the
/// BIP-143 sighash under that key, and (anchor) `1 OP_CSV` is satisfied by
/// a version-2 transaction with a height-based nSequence >= 1. The tests
/// additionally run libbitcoinconsensus over the same transactions.
fn verify_spend(tx: &Transaction, prevouts: &[TxOut]) -> Result<(), String> {
    if tx.input.len() != prevouts.len() {
        return Err("input count".into());
    }
    let secp = Secp256k1::verification_only();
    let mut cache = SighashCache::new(tx);
    for (i, (input, prev)) in tx.input.iter().zip(prevouts).enumerate() {
        if !input.script_sig.is_empty() || input.witness.len() != 2 {
            return Err(format!("input {i}: witness shape"));
        }
        let (sig_bytes, second) = (&input.witness[0], &input.witness[1]);
        let sig = ecdsa::Signature::from_slice(sig_bytes)
            .map_err(|_| format!("input {i}: signature encoding"))?;
        if sig.sighash_type != EcdsaSighashType::All {
            return Err(format!("input {i}: sighash type"));
        }
        let (pk, sighash) = if prev.script_pubkey.is_p2wsh() {
            let ws = ScriptBuf::from_bytes(second.to_vec());
            if ws.to_p2wsh() != prev.script_pubkey {
                return Err(format!("input {i}: witness script hash"));
            }
            // <33-byte key> OP_CHECKSIGVERIFY OP_1 OP_CSV
            let b = ws.as_bytes();
            if b.len() != 37 || b[0] != 0x21 || b[34..] != [0xad, 0x51, 0xb2] {
                return Err(format!("input {i}: not the anchor to_remote script"));
            }
            let csv_ok = tx.version >= Version::TWO
                && input.sequence.is_relative_lock_time()
                && input.sequence.is_height_locked()
                && (input.sequence.0 & 0xffff) >= 1;
            if !csv_ok {
                return Err(format!("input {i}: CSV not satisfied"));
            }
            let pk = PublicKey::from_slice(&b[1..34]).map_err(|_| format!("input {i}: key"))?;
            let h = cache
                .p2wsh_signature_hash(i, &ws, prev.value, EcdsaSighashType::All)
                .map_err(|_| format!("input {i}: sighash"))?;
            (pk, h)
        } else if prev.script_pubkey.is_p2wpkh() {
            let pk = PublicKey::from_slice(second).map_err(|_| format!("input {i}: key"))?;
            if ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash()) != prev.script_pubkey
            {
                return Err(format!("input {i}: key hash"));
            }
            let h = cache
                .p2wpkh_signature_hash(i, &prev.script_pubkey, prev.value, EcdsaSighashType::All)
                .map_err(|_| format!("input {i}: sighash"))?;
            (pk, h)
        } else {
            return Err(format!("input {i}: unsupported prevout"));
        };
        secp.verify_ecdsa(
            &Message::from_digest(sighash.to_byte_array()),
            &sig.signature,
            &pk,
        )
        .map_err(|_| format!("input {i}: bad signature"))?;
    }
    let out: u64 = tx.output.iter().map(|o| o.value.to_sat()).sum();
    let inp: u64 = prevouts.iter().map(|o| o.value.to_sat()).sum();
    if out > inp {
        return Err("outputs exceed inputs".into());
    }
    Ok(())
}

/// bitcoind `sendrawtransaction` broadcaster.
pub struct BitcoindBroadcast {
    rpc: Rpc,
}

impl BitcoindBroadcast {
    pub fn new(url: &str, user: &str, pass: &str) -> Self {
        Self {
            rpc: Rpc::new(url, user, pass, Duration::from_secs(30)),
        }
    }
}

impl Broadcaster for BitcoindBroadcast {
    fn broadcast(&self, tx: &Transaction) -> Result<Txid, LnError> {
        let r = self
            .rpc
            .call("sendrawtransaction", serde_json::json!([serialize_hex(tx)]))
            .map_err(|e| LnError::Other(format!("broadcast_failed: {e}")))?;
        let s = r
            .as_str()
            .ok_or_else(|| LnError::Other("broadcast_failed: malformed response".into()))?;
        returned_txid(s, tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Instant;

    use ldk_node::lightning::sign::KeysManager;

    const SEED: [u8; 64] = [7u8; 64];

    fn op(n: u8) -> OutPoint {
        OutPoint {
            txid: Txid::from_byte_array([n; 32]),
            vout: u32::from(n),
        }
    }

    /// The scripts as ldk-node 0.7.1 itself gets them: `builder.rs` takes
    /// the BIP-32 master of the 64 seed bytes (`config.network`) and builds
    /// `KeysManager::new(master.private_key, now_secs, now_nanos, true)`.
    fn ldk_spks(seed: &[u8; 64], network: Network, secs: u64, nanos: u32) -> Vec<ScriptBuf> {
        let master = Xpriv::new_master(network, seed).unwrap();
        let km = KeysManager::new(&master.private_key.secret_bytes(), secs, nanos, true);
        km.possible_v2_counterparty_closed_balance_spks(&Secp256k1::new())
    }

    #[test]
    fn static_scripts_are_deterministic_and_seed_specific() {
        let a = static_scripts(&SEED, Network::Regtest);
        assert_eq!(a.len(), 2000);
        assert_eq!(STATIC_SCRIPT_COUNT, 2000);
        assert!(a.iter().enumerate().all(|(i, (idx, _))| *idx as usize == i));
        assert_eq!(a, static_scripts(&SEED, Network::Regtest));
        assert_eq!(
            a,
            static_scripts(&SEED, Network::Bitcoin),
            "network-independent"
        );
        let ours: Vec<ScriptBuf> = a.iter().map(|(_, s)| s.clone()).collect();
        // Exactly LDK's list, whatever the KeysManager starting time / network.
        assert_eq!(ours, ldk_spks(&SEED, Network::Regtest, 0, 0));
        assert_eq!(
            ours,
            ldk_spks(&SEED, Network::Bitcoin, 1_793_000_000, 123_456_789)
        );
        let set: HashSet<&ScriptBuf> = ours.iter().collect();
        assert_eq!(set.len(), 2000, "all distinct");
        assert!(a.iter().all(|(i, s)| if i.is_multiple_of(2) {
            s.is_p2wpkh()
        } else {
            s.is_p2wsh()
        }));
        let b = static_scripts(&[8u8; 64], Network::Regtest);
        assert_eq!(b.len(), 2000);
        assert!(
            b.iter().all(|(_, s)| !set.contains(s)),
            "another seed shares no script"
        );
    }

    type SrcFn<'a> = Box<dyn Fn(&[ScriptBuf]) -> Result<Vec<(usize, UtxoRef)>, LnError> + 'a>;
    struct FnSource<'a> {
        batch: usize,
        f: SrcFn<'a>,
    }
    impl ScriptUtxoSource for FnSource<'_> {
        fn batch_size(&self) -> usize {
            self.batch
        }
        fn utxos_for_scripts(&self, s: &[ScriptBuf]) -> Result<Vec<(usize, UtxoRef)>, LnError> {
            (self.f)(s)
        }
    }

    /// A source holding `have` = (script index, outpoint tag, value).
    fn source_with(batch: usize, have: &[(u32, u8, u64)]) -> FnSource<'static> {
        let scripts = static_scripts(&SEED, Network::Regtest);
        let map: HashMap<ScriptBuf, Vec<(u8, u64)>> =
            have.iter().fold(HashMap::new(), |mut m, &(i, n, v)| {
                m.entry(scripts[i as usize].1.clone())
                    .or_default()
                    .push((n, v));
                m
            });
        FnSource {
            batch,
            f: Box::new(move |s| {
                Ok(s.iter()
                    .enumerate()
                    .flat_map(|(i, spk)| {
                        map.get(spk).into_iter().flatten().map(move |&(n, v)| {
                            (
                                i,
                                UtxoRef {
                                    outpoint: op(n),
                                    value_sat: v,
                                    confirmations: 3,
                                },
                            )
                        })
                    })
                    .collect())
            }),
        }
    }

    #[test]
    fn scan_reports_progress_and_maps_indices() {
        let src = source_with(300, &[(1501, 2, 70_000), (0, 1, 50_000), (1999, 3, 1_000)]);
        let mut seen = vec![];
        let got = scan_static_outputs(&SEED, Network::Regtest, &src, &mut |d, t| seen.push((d, t)))
            .unwrap();
        let want_progress: Vec<(u32, u32)> = [0, 300, 600, 900, 1200, 1500, 1800, 2000]
            .iter()
            .map(|&d| (d, 2000))
            .collect();
        assert_eq!(seen, want_progress);
        let mut want = vec![
            StaticUtxo {
                outpoint: op(1),
                value_sat: 50_000,
                script_index: 0,
                confirmations: 3,
            },
            StaticUtxo {
                outpoint: op(2),
                value_sat: 70_000,
                script_index: 1501,
                confirmations: 3,
            },
            StaticUtxo {
                outpoint: op(3),
                value_sat: 1_000,
                script_index: 1999,
                confirmations: 3,
            },
        ];
        want.sort_by_key(|u| u.outpoint);
        assert_eq!(got, want);

        // A batch size larger than the list, or zero, still works.
        for batch in [0, 5000] {
            let src = source_with(batch, &[(7, 9, 10_000)]);
            let got = scan_static_outputs(&SEED, Network::Regtest, &src, &mut |_, _| {}).unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].script_index, 7);
        }
        let none = source_with(100, &[]);
        assert!(
            scan_static_outputs(&SEED, Network::Regtest, &none, &mut |_, _| {})
                .unwrap()
                .is_empty()
        );
    }

    fn fixed(batch: usize, out: Vec<(usize, UtxoRef)>) -> FnSource<'static> {
        FnSource {
            batch,
            f: Box::new(move |_| Ok(out.clone())),
        }
    }

    fn uref(n: u8, v: u64) -> UtxoRef {
        UtxoRef {
            outpoint: op(n),
            value_sat: v,
            confirmations: 1,
        }
    }

    fn scan_err(src: &FnSource) -> String {
        match scan_static_outputs(&SEED, Network::Regtest, src, &mut |_, _| {}) {
            Err(LnError::Other(s)) => s,
            other => panic!("expected scan_failed, got {other:?}"),
        }
    }

    #[test]
    fn scan_survives_adversarial_source_data() {
        // Index out of range for the batch.
        assert!(scan_err(&fixed(2000, vec![(2000, uref(1, 1000))])).starts_with("scan_failed"));
        assert!(scan_err(&fixed(100, vec![(100, uref(1, 1000))])).starts_with("scan_failed"));
        assert!(scan_err(&fixed(100, vec![(usize::MAX, uref(1, 1000))])).starts_with("scan_failed"));
        // Absurd value.
        assert!(
            scan_err(&fixed(2000, vec![(0, uref(1, MAX_MONEY_SAT + 1))]))
                .starts_with("scan_failed")
        );
        assert!(scan_err(&fixed(2000, vec![(0, uref(1, u64::MAX))])).starts_with("scan_failed"));
        // One outpoint, two different scripts or values.
        assert!(
            scan_err(&fixed(2000, vec![(0, uref(1, 1000)), (1, uref(1, 1000))]))
                .starts_with("scan_failed")
        );
        assert!(
            scan_err(&fixed(2000, vec![(0, uref(1, 1000)), (0, uref(1, 999))]))
                .starts_with("scan_failed")
        );
        // The same outpoint reported in every batch (index 0 of each) = different scripts.
        assert!(scan_err(&fixed(100, vec![(0, uref(1, 1000))])).starts_with("scan_failed"));
        // Source errors propagate.
        let failing = FnSource {
            batch: 100,
            f: Box::new(|_| Err(LnError::Other("scan_failed: down".into()))),
        };
        assert_eq!(scan_err(&failing), "scan_failed: down");

        // Exact duplicates collapse; zero values are dropped.
        let got = scan_static_outputs(
            &SEED,
            Network::Regtest,
            &fixed(
                2000,
                vec![(4, uref(1, 1000)), (4, uref(1, 1000)), (5, uref(2, 0))],
            ),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(
            got,
            vec![StaticUtxo {
                outpoint: op(1),
                value_sat: 1000,
                script_index: 4,
                confirmations: 1
            }]
        );
    }

    #[derive(Default)]
    struct FakeBroadcast {
        sent: RefCell<Vec<Transaction>>,
        fail: Option<String>,
        wrong_txid: bool,
    }
    impl Broadcaster for FakeBroadcast {
        fn broadcast(&self, tx: &Transaction) -> Result<Txid, LnError> {
            self.sent.borrow_mut().push(tx.clone());
            if let Some(e) = &self.fail {
                return Err(LnError::Other(e.clone()));
            }
            if self.wrong_txid {
                return Ok(Txid::all_zeros());
            }
            Ok(tx.compute_txid())
        }
    }

    fn dest(network: Network) -> Address {
        let secp = Secp256k1::new();
        let sk = SecretKey::from_slice(&[0x42; 32]).unwrap();
        Address::p2wpkh(
            &CompressedPublicKey(PublicKey::from_secret_key(&secp, &sk)),
            network,
        )
    }

    fn su(n: u8, value_sat: u64, script_index: u32) -> StaticUtxo {
        StaticUtxo {
            outpoint: op(n),
            value_sat,
            script_index,
            confirmations: 1,
        }
    }

    fn prevouts(utxos: &[StaticUtxo]) -> HashMap<OutPoint, TxOut> {
        let scripts = static_scripts(&SEED, Network::Regtest);
        utxos
            .iter()
            .map(|u| {
                (
                    u.outpoint,
                    TxOut {
                        value: Amount::from_sat(u.value_sat),
                        script_pubkey: scripts[u.script_index as usize].1.clone(),
                    },
                )
            })
            .collect()
    }

    fn sweep(
        utxos: &[StaticUtxo],
        feerate: u64,
        b: &FakeBroadcast,
    ) -> Result<Option<Txid>, LnError> {
        sweep_static_outputs(
            &SEED,
            Network::Regtest,
            utxos,
            &dest(Network::Regtest),
            feerate,
            b,
        )
    }

    #[test]
    fn sweep_builds_valid_signed_tx_for_known_utxo() {
        // An anchor (P2WSH) and a static_remote_key (P2WPKH) output, plus a
        // high key index.
        let utxos = [su(9, 100_000, 1), su(3, 50_000, 4), su(200, 25_000, 1555)];
        let b = FakeBroadcast::default();
        let txid = sweep(&utxos, 3, &b).unwrap().expect("a sweep");
        let sent = b.sent.borrow();
        assert_eq!(sent.len(), 1);
        let tx = &sent[0];
        assert_eq!(tx.compute_txid(), txid);
        assert_eq!(tx.version, Version::TWO);
        assert_eq!(tx.lock_time, LockTime::ZERO);
        let ins: Vec<OutPoint> = tx.input.iter().map(|i| i.previous_output).collect();
        let mut sorted = vec![op(3), op(9), op(200)];
        sorted.sort();
        assert_eq!(ins, sorted, "inputs sorted by outpoint");
        assert!(tx
            .input
            .iter()
            .all(|i| i.sequence == Sequence(1) && i.script_sig.is_empty()));
        assert!(
            tx.input
                .iter()
                .all(|i| *i.witness.nth(0).unwrap().last().unwrap() == 0x01),
            "SIGHASH_ALL"
        );
        assert_eq!(tx.output.len(), 1);
        assert_eq!(
            tx.output[0].script_pubkey,
            dest(Network::Regtest).script_pubkey()
        );
        let fee = 175_000 - tx.output[0].value.to_sat();
        assert_eq!(
            fee,
            tx.weight().to_wu().div_ceil(4) * 3,
            "fee = vsize × feerate"
        );

        // Real consensus verification (libbitcoinconsensus) against the prevouts.
        let prev = prevouts(&utxos);
        tx.verify(|o| prev.get(o).cloned())
            .expect("consensus-valid spend");
        // ... and it really checks: a wrong prevout amount fails.
        let mut bad = prev.clone();
        bad.get_mut(&op(9)).unwrap().value = Amount::from_sat(100_001);
        assert!(tx.verify(|o| bad.get(o).cloned()).is_err());
        // Our own independent check agrees, and rejects a tampered signature.
        let ordered: Vec<TxOut> = tx
            .input
            .iter()
            .map(|i| prev[&i.previous_output].clone())
            .collect();
        verify_spend(tx, &ordered).expect("independent check");
        let mut t2 = tx.clone();
        let mut w: Vec<Vec<u8>> = t2.input[0].witness.iter().map(<[u8]>::to_vec).collect();
        w[0][10] ^= 1;
        t2.input[0].witness = Witness::from_slice(&w);
        assert!(verify_spend(&t2, &ordered).is_err());
        assert!(t2.verify(|o| prev.get(o).cloned()).is_err());
    }

    #[test]
    fn sweep_fee_is_exact_for_known_input_count() {
        // One anchor input → P2WPKH: base 82 B (328 WU) + witness 113 WU
        // (marker/flag 2, item count 1, 71-byte low-R sig + push 72,
        // 37-byte script + push 38) = 441 WU → 111 vB.
        for (feerate, fee) in [(1u64, 111u64), (2, 222), (25, 2775)] {
            let b = FakeBroadcast::default();
            sweep(&[su(1, 100_000, 1)], feerate, &b).unwrap().unwrap();
            let tx = &b.sent.borrow()[0];
            assert_eq!(tx.weight().to_wu(), 441);
            assert_eq!(tx.output[0].value.to_sat(), 100_000 - fee);
        }
        // Two anchor inputs: base 123 B (492 WU) + witness 2 + 2×111 = 716 WU → 179 vB.
        let b = FakeBroadcast::default();
        sweep(&[su(1, 100_000, 1), su(2, 100_000, 3)], 2, &b)
            .unwrap()
            .unwrap();
        let tx = &b.sent.borrow()[0];
        assert_eq!(tx.weight().to_wu(), 716);
        assert_eq!(tx.output[0].value.to_sat(), 200_000 - 358);
    }

    #[test]
    fn sweep_returns_none_when_dust_or_fee_exceeds_value() {
        let b = FakeBroadcast::default();
        // 111 vB × 2 = 222 sat fee.
        assert_eq!(
            sweep(&[su(1, 767, 1)], 2, &b).unwrap(),
            None,
            "net 545 < 546"
        );
        assert_eq!(
            sweep(&[su(1, 200, 1)], 10, &b).unwrap(),
            None,
            "fee 1110 > value"
        );
        assert_eq!(
            sweep(&[su(1, 1110, 1)], 10, &b).unwrap(),
            None,
            "fee = value"
        );
        assert_eq!(sweep(&[], 2, &b).unwrap(), None, "nothing to spend");
        let mut unconf = su(1, 100_000, 1);
        unconf.confirmations = 0;
        assert_eq!(sweep(&[unconf], 2, &b).unwrap(), None, "only unconfirmed");
        assert!(b.sent.borrow().is_empty());
        // Boundary: net exactly 546 is swept.
        assert!(sweep(&[su(1, 768, 1)], 2, &b).unwrap().is_some());
        assert_eq!(b.sent.borrow()[0].output[0].value.to_sat(), 546);
        // Unconfirmed inputs are left out of a sweep that has confirmed ones.
        let b = FakeBroadcast::default();
        sweep(&[su(1, 100_000, 1), unconf_at(2)], 2, &b)
            .unwrap()
            .unwrap();
        assert_eq!(b.sent.borrow()[0].input.len(), 1);
    }

    fn unconf_at(n: u8) -> StaticUtxo {
        StaticUtxo {
            confirmations: 0,
            ..su(n, 100_000, 3)
        }
    }

    #[test]
    fn sweep_rejects_wrong_network_address() {
        let b = FakeBroadcast::default();
        let u = [su(1, 100_000, 1)];
        for net in [Network::Bitcoin, Network::Testnet4] {
            let r = sweep_static_outputs(&SEED, Network::Regtest, &u, &dest(net), 2, &b);
            assert_eq!(r, Err(LnError::WrongNetwork), "{net}");
        }
        let r = sweep_static_outputs(&SEED, Network::Bitcoin, &u, &dest(Network::Regtest), 2, &b);
        assert_eq!(r, Err(LnError::WrongNetwork));
        for feerate in [0, MAX_FEERATE_SAT_PER_VB + 1, u64::MAX] {
            assert_eq!(
                sweep(&u, feerate, &b),
                Err(LnError::Other("invalid_feerate".into()))
            );
        }
        assert!(b.sent.borrow().is_empty());
        // The highest allowed feerate works.
        assert!(sweep(&[su(1, 10_000_000, 1)], MAX_FEERATE_SAT_PER_VB, &b)
            .unwrap()
            .is_some());
    }

    #[test]
    fn sweep_does_not_broadcast_on_signing_failure() {
        let b = FakeBroadcast::default();
        let cases: Vec<Vec<StaticUtxo>> = vec![
            vec![su(1, 100_000, STATIC_SCRIPT_COUNT)], // no such key
            vec![su(1, 100_000, 1), su(2, 100_000, u32::MAX)], // one bad among good
            vec![su(1, 100_000, 1), su(1, 100_000, 1)], // duplicate input
            vec![su(1, 100_000, 1), su(1, 50_000, 3)], // conflicting duplicate
            vec![su(1, 0, 1)],                         // zero value
            vec![su(1, MAX_MONEY_SAT + 1, 1)],         // absurd value
            (0..1500u32)
                .map(|vout| StaticUtxo {
                    outpoint: OutPoint {
                        txid: Txid::from_byte_array([1; 32]),
                        vout,
                    },
                    ..su(1, 1000, 1)
                })
                .collect(), // above the standard weight
        ];
        for (i, c) in cases.iter().enumerate() {
            let r = sweep(c, 1, &b);
            assert!(matches!(r, Err(LnError::Other(_))), "case {i}: {r:?}");
        }
        assert!(b.sent.borrow().is_empty(), "nothing broadcast");
    }

    #[test]
    fn broadcast_error_is_surfaced() {
        let u = [su(1, 100_000, 1)];
        let b = FakeBroadcast {
            fail: Some("connection refused".into()),
            ..Default::default()
        };
        assert_eq!(
            sweep(&u, 2, &b),
            Err(LnError::Other(
                "broadcast_failed: connection refused".into()
            ))
        );
        let b = FakeBroadcast {
            fail: Some("broadcast_failed: rpc: bad-txns".into()),
            ..Default::default()
        };
        assert_eq!(
            sweep(&u, 2, &b),
            Err(LnError::Other("broadcast_failed: rpc: bad-txns".into()))
        );
        let b = FakeBroadcast {
            wrong_txid: true,
            ..Default::default()
        };
        assert_eq!(
            sweep(&u, 2, &b),
            Err(LnError::Other("broadcast_failed: txid mismatch".into()))
        );
    }

    #[test]
    fn errors_and_debug_carry_no_secrets() {
        // Nothing holding key material implements Debug; the error strings
        // are fixed text. Check the master/static key bytes never appear.
        let master = Xpriv::new_master(Network::Testnet, &SEED).unwrap();
        let km = Xpriv::new_master(Network::Testnet, &master.private_key.secret_bytes()).unwrap();
        let secrets = [
            hex::encode(SEED),
            hex::encode(master.private_key.secret_bytes()),
            hex::encode(km.private_key.secret_bytes()),
        ];
        let b = FakeBroadcast {
            fail: Some("x".into()),
            ..Default::default()
        };
        let msgs = [
            format!("{:?}", sweep(&[su(1, 100_000, 1)], 2, &b)),
            format!("{:?}", sweep(&[su(1, 100_000, 99_999)], 2, &b)),
            format!(
                "{:?}",
                scan_static_outputs(
                    &SEED,
                    Network::Regtest,
                    &fixed(10, vec![(10, uref(1, 1))]),
                    &mut |_, _| {}
                )
            ),
        ];
        for m in &msgs {
            for s in &secrets {
                assert!(!m.contains(s.as_str()), "{m}");
            }
        }
    }

    // ---- Esplora ------------------------------------------------------

    #[test]
    fn esplora_scripthash_vector() {
        // Electrum protocol docs: P2PKH of 1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa.
        let s = ScriptBuf::from_hex("76a91462e907b15cbf27d5425399ebf6f0fb50ebb88f1888ac").unwrap();
        assert_eq!(
            esplora_scripthash(&s),
            "8b01df4e368ea28f8dc0423bcf7a4923e3a12d307c875e47a0cfbf90b5c39161"
        );
    }

    type Resp = Result<HttpResponse, String>;
    #[derive(Default)]
    struct FakeHttp {
        queued: Mutex<HashMap<String, VecDeque<Resp>>>,
        default: Mutex<HashMap<String, Resp>>,
        calls: Mutex<HashMap<String, usize>>,
        posts: Mutex<Vec<(String, String)>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        delay: Duration,
    }
    fn ok(body: &str) -> Resp {
        Ok(HttpResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
        })
    }
    fn status(code: u16) -> Resp {
        Ok(HttpResponse {
            status: code,
            body: b"err".to_vec(),
        })
    }
    impl FakeHttp {
        fn on(&self, url: &str, r: Resp) {
            self.default.lock().unwrap().insert(url.into(), r);
        }
        fn queue(&self, url: &str, rs: Vec<Resp>) {
            self.queued.lock().unwrap().insert(url.into(), rs.into());
        }
        fn calls(&self, url: &str) -> usize {
            self.calls.lock().unwrap().get(url).copied().unwrap_or(0)
        }
    }
    impl HttpClient for FakeHttp {
        fn get(&self, url: &str) -> Resp {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            *self.calls.lock().unwrap().entry(url.into()).or_default() += 1;
            let q = self
                .queued
                .lock()
                .unwrap()
                .get_mut(url)
                .and_then(VecDeque::pop_front);
            let r = q
                .or_else(|| self.default.lock().unwrap().get(url).cloned())
                .unwrap_or_else(|| ok("[]"));
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            r
        }
        fn post(&self, url: &str, _ct: &str, body: &str) -> Resp {
            self.posts.lock().unwrap().push((url.into(), body.into()));
            self.default
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_else(|| status(404))
        }
    }

    const BASE: &str = "https://esplora.test/api";
    fn utxo_url(s: &ScriptBuf) -> String {
        format!("{BASE}/scripthash/{}/utxo", esplora_scripthash(s))
    }
    fn esplora(http: &Arc<FakeHttp>) -> EsploraScan {
        EsploraScan::with_client(&format!("{BASE}/"), http.clone())
            .with_backoff(Duration::ZERO)
            .with_pause(Duration::ZERO)
    }
    fn scripts(n: usize) -> Vec<ScriptBuf> {
        static_scripts(&SEED, Network::Regtest)
            .into_iter()
            .take(n)
            .map(|(_, s)| s)
            .collect()
    }
    const TXID_A: &str = "aa00000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn esplora_scan_maps_utxos_and_empty_results() {
        let http = Arc::new(FakeHttp::default());
        http.on(&format!("{BASE}/blocks/tip/height"), ok("110"));
        let s = scripts(3);
        http.on(&utxo_url(&s[0]), ok(&format!(
            r#"[{{"txid":"{TXID_A}","vout":1,"value":199056,"status":{{"confirmed":true,"block_height":101}}}},
               {{"txid":"{TXID_A}","vout":2,"value":5000,"status":{{"confirmed":false}}}}]"#
        )));
        http.on(&utxo_url(&s[2]), status(404));
        let got = esplora(&http).utxos_for_scripts(&s).unwrap();
        let t: Txid = TXID_A.parse().unwrap();
        assert_eq!(
            got,
            vec![
                (
                    0,
                    UtxoRef {
                        outpoint: OutPoint { txid: t, vout: 1 },
                        value_sat: 199_056,
                        confirmations: 10
                    }
                ),
                (
                    0,
                    UtxoRef {
                        outpoint: OutPoint { txid: t, vout: 2 },
                        value_sat: 5000,
                        confirmations: 0
                    }
                ),
            ]
        );
        assert!(s.iter().all(|x| http.calls(&utxo_url(x)) == 1));
    }

    #[test]
    fn esplora_retries_429_and_5xx_with_a_bound() {
        let http = Arc::new(FakeHttp::default());
        http.on(&format!("{BASE}/blocks/tip/height"), ok("110"));
        let s = scripts(2);
        http.queue(
            &utxo_url(&s[0]),
            vec![status(429), status(503), Err("timeout".into()), ok("[]")],
        );
        assert_eq!(esplora(&http).utxos_for_scripts(&s).unwrap(), vec![]);
        assert_eq!(http.calls(&utxo_url(&s[0])), 4);

        // Always 500: 1 try + 3 retries, then the scan fails.
        let http = Arc::new(FakeHttp::default());
        http.on(&format!("{BASE}/blocks/tip/height"), ok("110"));
        http.on(&utxo_url(&s[1]), status(500));
        let r = esplora(&http).utxos_for_scripts(&s);
        assert!(
            matches!(&r, Err(LnError::Other(m)) if m.starts_with("scan_failed:")),
            "{r:?}"
        );
        assert_eq!(http.calls(&utxo_url(&s[1])), 4);
    }

    #[test]
    fn esplora_malformed_responses_fail_without_retry() {
        let tip = format!("{BASE}/blocks/tip/height");
        let s = scripts(1);
        for body in [
            "{not json",
            r#"{"txid":"x"}"#,
            r#"[{"txid":"zz","vout":0,"value":1,"status":{"confirmed":true,"block_height":1}}]"#,
            &format!(
                r#"[{{"txid":"{TXID_A}","vout":4294967296,"value":1,"status":{{"confirmed":true,"block_height":1}}}}]"#
            ),
            &format!(
                r#"[{{"txid":"{TXID_A}","vout":0,"value":-1,"status":{{"confirmed":true,"block_height":1}}}}]"#
            ),
            &format!(r#"[{{"txid":"{TXID_A}","vout":0,"value":1}}]"#),
        ] {
            let http = Arc::new(FakeHttp::default());
            http.on(&tip, ok("110"));
            http.on(&utxo_url(&s[0]), ok(body));
            let r = esplora(&http).utxos_for_scripts(&s);
            assert!(
                matches!(&r, Err(LnError::Other(m)) if m.starts_with("scan_failed:")),
                "{body}: {r:?}"
            );
            assert_eq!(http.calls(&utxo_url(&s[0])), 1, "{body}");
        }
        // A broken tip endpoint (wrong base URL) fails the scan.
        let http = Arc::new(FakeHttp::default());
        http.on(&tip, status(404));
        assert!(esplora(&http).utxos_for_scripts(&s).is_err());
    }

    #[test]
    fn esplora_partial_failure_aborts_and_concurrency_is_bounded() {
        let http = Arc::new(FakeHttp {
            delay: Duration::from_millis(5),
            ..Default::default()
        });
        http.on(&format!("{BASE}/blocks/tip/height"), ok("110"));
        let s = scripts(EsploraScan::BATCH);
        let t = Instant::now();
        assert!(esplora(&http).utxos_for_scripts(&s).unwrap().is_empty());
        let max = http.max_in_flight.load(Ordering::SeqCst);
        assert!(
            max > 1 && max <= EsploraScan::CONCURRENCY,
            "max in flight {max}"
        );
        assert!(t.elapsed() < Duration::from_secs(5));

        http.on(&utxo_url(&s[17]), status(400));
        let r = esplora(&http).utxos_for_scripts(&s);
        assert!(
            matches!(&r, Err(LnError::Other(m)) if m.starts_with("scan_failed:")),
            "{r:?}"
        );
    }

    #[test]
    fn esplora_full_scan_reports_progress_per_batch() {
        let http = Arc::new(FakeHttp::default());
        http.on(&format!("{BASE}/blocks/tip/height"), ok("200"));
        let target = static_scripts(&SEED, Network::Regtest)[1201].1.clone();
        http.on(&utxo_url(&target), ok(&format!(
            r#"[{{"txid":"{TXID_A}","vout":0,"value":42000,"status":{{"confirmed":true,"block_height":200}}}}]"#
        )));
        let mut calls = 0;
        let got = scan_static_outputs(&SEED, Network::Regtest, &esplora(&http), &mut |_, _| {
            calls += 1
        })
        .unwrap();
        assert_eq!(calls, 1 + 2000 / EsploraScan::BATCH);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].script_index, got[0].value_sat, got[0].confirmations),
            (1201, 42_000, 1)
        );
    }

    fn some_tx() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: op(1),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: dest(Network::Regtest).script_pubkey(),
            }],
        }
    }

    #[test]
    fn esplora_broadcast_checks_the_returned_txid() {
        let tx = some_tx();
        let url = format!("{BASE}/tx");
        let http = Arc::new(FakeHttp::default());
        http.on(&url, ok(&format!("{}\n", tx.compute_txid())));
        let b = EsploraBroadcast::with_client(BASE, http.clone());
        assert_eq!(b.broadcast(&tx).unwrap(), tx.compute_txid());
        assert_eq!(
            http.posts.lock().unwrap()[0],
            (url.clone(), serialize_hex(&tx))
        );

        http.on(&url, ok(TXID_A));
        assert_eq!(
            b.broadcast(&tx),
            Err(LnError::Other("broadcast_failed: txid mismatch".into()))
        );
        http.on(&url, ok("not a txid"));
        assert!(
            matches!(b.broadcast(&tx), Err(LnError::Other(m)) if m.starts_with("broadcast_failed:"))
        );
        http.on(
            &url,
            Ok(HttpResponse {
                status: 400,
                body: b"sendrawtransaction RPC error: bad-txns-inputs-missingorspent".to_vec(),
            }),
        );
        assert_eq!(
            b.broadcast(&tx),
            Err(LnError::Other("broadcast_failed: http 400: sendrawtransaction RPC error: bad-txns-inputs-missingorspent".into()))
        );
        http.on(&url, Err("transport: Io".into()));
        assert_eq!(
            b.broadcast(&tx),
            Err(LnError::Other("broadcast_failed: transport: Io".into()))
        );
    }

    #[test]
    fn scantxoutset_result_maps_back_to_indices() {
        let s = scripts(4);
        let res = serde_json::json!({
            "success": true, "height": 120,
            "unspents": [
                {"txid": TXID_A, "vout": 1, "scriptPubKey": s[3].to_hex_string(), "amount": 0.00199056, "height": 111},
                {"txid": TXID_A, "vout": 0, "scriptPubKey": s[0].to_hex_string(), "amount": 21000000.0, "height": 120},
            ],
        });
        let t: Txid = TXID_A.parse().unwrap();
        assert_eq!(
            parse_scantxoutset(&res, &s).unwrap(),
            vec![
                (
                    3,
                    UtxoRef {
                        outpoint: OutPoint { txid: t, vout: 1 },
                        value_sat: 199_056,
                        confirmations: 10
                    }
                ),
                (
                    0,
                    UtxoRef {
                        outpoint: OutPoint { txid: t, vout: 0 },
                        value_sat: MAX_MONEY_SAT,
                        confirmations: 1
                    }
                ),
            ]
        );
        for bad in [
            serde_json::json!({"success": false, "height": 1, "unspents": []}),
            serde_json::json!({"success": true, "height": 1}),
            serde_json::json!({"success": true, "height": 1, "unspents": [{"txid": TXID_A, "vout": 0, "scriptPubKey": "0014", "amount": 1.0, "height": 1}]}),
            serde_json::json!({"success": true, "height": 1, "unspents": [{"txid": TXID_A, "vout": 0, "scriptPubKey": s[0].to_hex_string(), "amount": 21000000.1, "height": 1}]}),
            serde_json::json!({"success": true, "height": 1, "unspents": [{"txid": TXID_A, "vout": 0, "scriptPubKey": s[0].to_hex_string(), "amount": -1.0, "height": 1}]}),
            serde_json::json!({"success": true, "height": 1, "unspents": [{"txid": "zz", "vout": 0, "scriptPubKey": s[0].to_hex_string(), "amount": 1.0, "height": 1}]}),
        ] {
            assert!(
                matches!(parse_scantxoutset(&bad, &s), Err(LnError::Other(m)) if m.starts_with("scan_failed:")),
                "{bad}"
            );
        }
    }
}
