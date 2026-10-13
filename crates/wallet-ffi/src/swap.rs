//! One P2P BTC⇄XBT swap as seen from one phone. The XBT sender is always the
//! initiator. The swap's two signing secrets are passed in on construction and
//! never serialised: `to_json` persists everything else, and `from_json` needs
//! the same secrets back.

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use wallet_core::bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use wallet_core::bitcoin::hashes::{sha256, Hash};
use wallet_core::bitcoin::secp256k1::PublicKey;
use wallet_core::bitcoin::{Address, Amount, OutPoint, ScriptBuf, Transaction, Txid};
use wallet_core::htlc::preimage_from_witness;
use wallet_core::{
    network_params, params_from_terms, ChainParams, HtlcContract, SwapEvent, SwapKey, SwapMachine,
    SwapRole, SwapState, SwapTerms,
};

use crate::{err, parse_address, spk, Result};

#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct HtlcInfo {
    /// The HTLC's P2WSH address — what the funding transaction pays.
    pub address: String,
    pub amount_sat: u64,
    pub script_pubkey_hex: String,
}

/// A party's two swap pubkeys, hex compressed.
#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct SwapPubkeys {
    pub xbt: String,
    pub btc: String,
}

/// A redeem signed before the preimage is known (see `wallet_core::PresignedRedeem`).
#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct FfiPresignedRedeem {
    pub tx_hex: String,
    pub sig_hex: String,
    pub pubkey_hex: String,
    pub witness_script_hex: String,
}

/// Everything about the swap except the secrets — what `to_json` writes.
#[derive(Serialize, Deserialize)]
struct Persisted {
    terms: SwapTerms,
    role: SwapRole,
    our_xbt: PublicKey,
    our_btc: PublicKey,
    /// Where our redeem pays (receive chain).
    redeem_address: String,
    /// Where our refund pays (send chain).
    refund_address: String,
    /// Counterparty `(xbt, btc)` pubkeys and the hashlock, once exchanged.
    their: Option<(PublicKey, PublicKey)>,
    hashlock: Option<sha256::Hash>,
    machine: Option<SwapMachine>,
    /// The initiator's preimage nonce (hex, 16 bytes), drawn by the phone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preimage_nonce: Option<String>,
}

/// The initiator's 16-byte preimage nonce from hex; the participant has none.
fn parse_nonce(role: SwapRole, hex_nonce: &str) -> Result<Option<[u8; 16]>> {
    match role {
        SwapRole::Participant if hex_nonce.is_empty() => Ok(None),
        SwapRole::Participant => Err(err("the participant has no preimage nonce")),
        SwapRole::Initiator => hex::decode(hex_nonce)
            .ok()
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map(Some)
            .ok_or_else(|| err("preimage nonce must be 16 bytes of hex")),
    }
}

fn parse_terms(terms_json: &str) -> Result<SwapTerms> {
    serde_json::from_str(terms_json).map_err(|e| err(format!("swap terms: {e}")))
}

fn parse_role(role: &str) -> Result<SwapRole> {
    match role {
        "initiator" => Ok(SwapRole::Initiator),
        "participant" => Ok(SwapRole::Participant),
        other => Err(err(format!("role must be \"initiator\" or \"participant\", not {other:?}"))),
    }
}

/// Check matched terms against the user's limit price (`wallet_core::validate_terms`),
/// and that they are for `network` — the phone's own ("bitcoin" | "regtest"): the
/// edge chooses the terms' network. `role` decides the direction: "initiator" (XBT
/// sender) accepts prices at or above the limit, "participant" (BTC sender) prices
/// at or below it.
#[uniffi::export]
pub fn validate_swap_terms(
    terms_json: String,
    limit_price_sat_per_xbt: u64,
    role: String,
    now: u32,
    network: String,
) -> Result<()> {
    let terms = parse_terms(&terms_json)?;
    let role = parse_role(&role)?;
    wallet_core::swap_terms::validate_terms_on(&terms, &network, limit_price_sat_per_xbt, role, now)?;
    Ok(())
}

/// Latest unix time the participant may still fund its BTC HTLC
/// (`wallet_core::swap_terms::participant_fund_latest`).
#[uniffi::export]
pub fn participant_fund_latest(terms_json: String) -> Result<u32> {
    Ok(wallet_core::swap_terms::participant_fund_latest(&parse_terms(&terms_json)?))
}

/// The commission one side pays on `send_sat`: 0.5%, but at least 400 sat
/// (`wallet_core::swap_terms::swap_commission_sat`).
#[uniffi::export]
pub fn swap_commission_sat(send_sat: u64) -> u64 {
    wallet_core::swap_terms::swap_commission_sat(send_sat)
}

/// The highest feerate (sat/vB) a swap transaction is built at: claims and
/// refunds are capped here by the core, and the phone clamps the edge's fee
/// estimates and its swap funding to it too
/// (`wallet_core::swap::MAX_SWAP_FEERATE_SAT_VB`).
#[uniffi::export]
pub fn max_swap_feerate_sat_vb() -> u64 {
    wallet_core::swap::MAX_SWAP_FEERATE_SAT_VB
}

/// The txid of a raw transaction (hex): what a funding broadcast will return,
/// known before it is sent.
#[uniffi::export]
pub fn txid_of(tx_hex: String) -> Result<String> {
    let tx: Transaction = deserialize_hex(&tx_hex).map_err(|e| err(format!("bad transaction: {e}")))?;
    Ok(tx.compute_txid().to_string())
}

#[derive(uniffi::Object)]
pub struct SwapSession {
    /// Copied out of `inner`: it never changes, and the key/params helpers that
    /// read it run while `inner` is locked.
    role: SwapRole,
    /// The terms' match id (fixed): the initiator's preimage is bound to it.
    match_id: String,
    /// The initiator's preimage nonce (fixed); `None` for the participant.
    nonce: Option<[u8; 16]>,
    xbt_key: SwapKey,
    btc_key: SwapKey,
    btc_params: ChainParams,
    xbt_params: ChainParams,
    inner: Mutex<Persisted>,
}

#[uniffi::export]
impl SwapSession {
    /// `role` is `"initiator"` (sends XBT) or `"participant"` (sends BTC). The
    /// secrets come from `Wallet::swap_secret` for this swap's index.
    /// `preimage_nonce`: the initiator's 16 random bytes (hex) that go into its
    /// preimage — fresh for a new swap, or as relayed with the hashlock for one
    /// rebuilt after a restore; empty for the participant.
    #[uniffi::constructor]
    pub fn new(
        terms_json: String,
        role: String,
        xbt_secret: Vec<u8>,
        btc_secret: Vec<u8>,
        redeem_address: String,
        refund_address: String,
        preimage_nonce: String,
    ) -> Result<Arc<Self>> {
        let terms = parse_terms(&terms_json)?;
        let role = parse_role(&role)?;
        let nonce = parse_nonce(role, &preimage_nonce)?;
        let (btc_params, xbt_params) = network_params(&terms.network)?;
        let xbt_key = swap_key(&xbt_params, xbt_secret)?;
        let btc_key = swap_key(&btc_params, btc_secret)?;
        // the redeem pays out on the receive chain, the refund on the send chain
        let (recv, send) = match role {
            SwapRole::Initiator => (&btc_params, &xbt_params),
            SwapRole::Participant => (&xbt_params, &btc_params),
        };
        let redeem_address = parse_address(&redeem_address, recv.network)?.to_string();
        let refund_address = parse_address(&refund_address, send.network)?.to_string();
        let inner = Persisted {
            terms,
            role,
            our_xbt: xbt_key.pubkey(),
            our_btc: btc_key.pubkey(),
            redeem_address,
            refund_address,
            their: None,
            hashlock: None,
            machine: None,
            preimage_nonce: nonce.map(hex::encode),
        };
        Ok(Arc::new(SwapSession {
            role: inner.role,
            match_id: inner.terms.match_id.clone(),
            nonce,
            xbt_key,
            btc_key,
            btc_params,
            xbt_params,
            inner: Mutex::new(inner),
        }))
    }

    /// Resume from `to_json`. The secrets must be the ones the session was made with.
    #[uniffi::constructor]
    pub fn from_json(json: String, xbt_secret: Vec<u8>, btc_secret: Vec<u8>) -> Result<Arc<Self>> {
        let inner: Persisted =
            serde_json::from_str(&json).map_err(|e| err(format!("swap session: {e}")))?;
        let (btc_params, xbt_params) = network_params(&inner.terms.network)?;
        let xbt_key = swap_key(&xbt_params, xbt_secret)?;
        let btc_key = swap_key(&btc_params, btc_secret)?;
        if xbt_key.pubkey() != inner.our_xbt || btc_key.pubkey() != inner.our_btc {
            return Err(err("swap secrets do not match this swap session"));
        }
        // A session saved before nonces existed still opens (its refund needs
        // no preimage); only a new one must have its nonce.
        let nonce = match inner.preimage_nonce.as_deref() {
            None => None,
            Some(h) => parse_nonce(inner.role, h)?,
        };
        Ok(Arc::new(SwapSession {
            role: inner.role,
            match_id: inner.terms.match_id.clone(),
            nonce,
            xbt_key,
            btc_key,
            btc_params,
            xbt_params,
            inner: Mutex::new(inner),
        }))
    }

    /// Everything but the secrets.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&*self.lock()).expect("swap session serialises")
    }

    pub fn our_pubkeys(&self) -> SwapPubkeys {
        SwapPubkeys {
            xbt: hex::encode(self.xbt_key.pubkey().serialize()),
            btc: hex::encode(self.btc_key.pubkey().serialize()),
        }
    }

    /// Hex `sha256(preimage)` for the initiator; `None` for the participant.
    pub fn hashlock(&self) -> Option<String> {
        self.our_preimage().map(|p| sha256::Hash::hash(&p).to_string())
    }

    /// The initiator's preimage nonce (hex), relayed with the hashlock so a
    /// restore can rebuild the preimage; `None` for the participant.
    pub fn preimage_nonce(&self) -> Option<String> {
        self.nonce.map(hex::encode)
    }

    /// The initiator's preimage (hex); `None` for the participant.
    pub fn preimage(&self) -> Option<String> {
        self.our_preimage().map(hex::encode)
    }

    /// Record the counterparty's pubkeys and the hashlock, and build the swap.
    /// Calling it again with the same values is a no-op; different values are refused.
    pub fn set_counterparty(&self, their: SwapPubkeys, hashlock_hex: String) -> Result<()> {
        let their = (pubkey(&their.xbt)?, pubkey(&their.btc)?);
        let hashlock = sha256::Hash::from_str(&hashlock_hex).map_err(|_| err("bad hashlock"))?;
        if let Some(own) = self.our_preimage() {
            if sha256::Hash::hash(&own) != hashlock {
                return Err(err("hashlock does not match our preimage"));
            }
        }
        let mut s = self.lock();
        let ours = [s.our_xbt, s.our_btc];
        if ours.contains(&their.0) || ours.contains(&their.1) {
            return Err(err("the counterparty's keys are our own"));
        }
        if s.machine.is_some() {
            return if s.their == Some(their) && s.hashlock == Some(hashlock) {
                Ok(())
            } else {
                Err(err("counterparty already set to different keys"))
            };
        }
        let swap_id: [u8; 16] = sha256::Hash::hash(s.terms.match_id.as_bytes()).to_byte_array()[..16]
            .try_into()
            .expect("16 bytes");
        let params = params_from_terms(&s.terms, s.role, (s.our_xbt, s.our_btc), their, hashlock, swap_id);
        s.machine = Some(SwapMachine::new(params));
        s.their = Some(their);
        s.hashlock = Some(hashlock);
        Ok(())
    }

    /// The HTLC we fund (send chain).
    pub fn our_htlc(&self) -> Result<HtlcInfo> {
        let send = self.send_params().clone();
        let mut s = self.lock();
        let c = machine(&mut s)?.build_our_contract(&send)?.clone();
        Ok(htlc_info(&c, &send))
    }

    /// The HTLC the counterparty must fund (receive chain).
    pub fn their_htlc(&self) -> Result<HtlcInfo> {
        let recv = self.recv_params();
        let mut s = self.lock();
        Ok(htlc_info(&machine(&mut s)?.expected_their_contract(recv), recv))
    }

    /// Check one funding output of the counterparty's (a txout, from the chain
    /// backend): exactly their HTLC's script, exactly the agreed amount.
    pub fn verify_their_funding(&self, script_pubkey_hex: String, value_sat: u64) -> Result<()> {
        let script = spk(&script_pubkey_hex)?;
        let recv = self.recv_params();
        let mut s = self.lock();
        machine(&mut s)?.verify_their_output(&script, Amount::from_sat(value_sat), recv)?;
        Ok(())
    }

    pub fn record_their_outpoint(&self, txid: String, vout: u32) -> Result<()> {
        let op = outpoint(&txid, vout)?;
        machine(&mut self.lock())?.set_their_outpoint(op);
        Ok(())
    }

    pub fn record_our_outpoint(&self, txid: String, vout: u32) -> Result<()> {
        let op = outpoint(&txid, vout)?;
        machine(&mut self.lock())?.set_our_outpoint(op);
        Ok(())
    }

    /// Refunds of our HTLC to `refund_address`, one per feerate (sat/vB), as hex.
    pub fn signed_refund(&self, feerates: Vec<u64>) -> Result<Vec<String>> {
        let send = self.send_params().clone();
        let mut s = self.lock();
        let to = self.address_spk(&s.refund_address, &send)?;
        let m = machine(&mut s)?;
        let op = m.our_outpoint().ok_or_else(|| err("our funding outpoint is not recorded"))?;
        m.build_our_contract(&send)?;
        let txs = m.signed_refund(op, to, self.send_key(), &feerates)?;
        Ok(txs.iter().map(serialize_hex).collect())
    }

    /// Redeems of their HTLC to `redeem_address`, one per feerate, as hex. The
    /// initiator passes its own preimage.
    pub fn signed_redeem(&self, preimage_hex: String, feerates: Vec<u64>) -> Result<Vec<String>> {
        let preimage: [u8; 32] = hex::decode(&preimage_hex)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| err("preimage must be 32 bytes of hex"))?;
        let recv = self.recv_params();
        let mut s = self.lock();
        let to = self.address_spk(&s.redeem_address, recv)?;
        let m = machine(&mut s)?;
        let op = m.their_outpoint().ok_or_else(|| err("their funding outpoint is not recorded"))?;
        let txs = m.signed_redeem(op, to, self.recv_key(), &preimage, &feerates)?;
        Ok(txs.iter().map(serialize_hex).collect())
    }

    /// Releases of our HTLC (hex, unsigned) at `feerates`, paying our refund
    /// address: asked of the counterparty when the match is cancelled before
    /// it funded. Fails for a contract without a release (v1).
    pub fn release_txs(&self, feerates: Vec<u64>) -> Result<Vec<String>> {
        let send = self.send_params().clone();
        let mut s = self.lock();
        let to = self.address_spk(&s.refund_address, &send)?;
        let m = machine(&mut s)?;
        let op = m.our_outpoint().ok_or_else(|| err("our funding outpoint is not recorded"))?;
        m.build_our_contract(&send)?;
        Ok(m.release_txs(op, to, &feerates)?.iter().map(serialize_hex).collect())
    }

    /// Our signature (hex) on `tx_hex`, a release of their HTLC at
    /// `txid:vout` back to them. Sign only once this swap can never be funded
    /// by us: the counterparty then has its coins back.
    pub fn sign_release(&self, tx_hex: String, txid: String, vout: u32) -> Result<String> {
        let tx: Transaction = deserialize_hex(&tx_hex).map_err(|e| err(format!("bad release: {e}")))?;
        let op = outpoint(&txid, vout)?;
        let recv = self.recv_params().clone();
        let mut s = self.lock();
        let sig = machine(&mut s)?.sign_their_release(&tx, op, self.recv_key(), &recv)?;
        Ok(hex::encode(sig))
    }

    /// `tx_hex` (one of [`Self::release_txs`]) completed with the
    /// counterparty's signature and ours (hex). Refused unless it is exactly
    /// a release of our HTLC to our refund address and their signature
    /// verifies.
    pub fn finish_release(&self, tx_hex: String, their_sig_hex: String) -> Result<String> {
        let tx: Transaction = deserialize_hex(&tx_hex).map_err(|e| err(format!("bad release: {e}")))?;
        let sig = hex::decode(&their_sig_hex).map_err(|_| err("bad signature"))?;
        let send = self.send_params().clone();
        let mut s = self.lock();
        let to = self.address_spk(&s.refund_address, &send)?;
        let m = machine(&mut s)?;
        let op = m.our_outpoint().ok_or_else(|| err("our funding outpoint is not recorded"))?;
        m.build_our_contract(&send)?;
        Ok(serialize_hex(&m.finish_release(&tx, op, &to, &sig, self.send_key())?))
    }

    /// Redeems of their HTLC signed now, finished later once the preimage appears.
    pub fn presigned_redeem(&self, feerates: Vec<u64>) -> Result<Vec<FfiPresignedRedeem>> {
        let recv = self.recv_params();
        let mut s = self.lock();
        let to = self.address_spk(&s.redeem_address, recv)?;
        let m = machine(&mut s)?;
        let op = m.their_outpoint().ok_or_else(|| err("their funding outpoint is not recorded"))?;
        Ok(m
            .presign_redeem(op, to, self.recv_key(), &feerates)?
            .into_iter()
            .map(|p| FfiPresignedRedeem {
                tx_hex: p.tx_hex,
                sig_hex: p.sig_hex,
                pubkey_hex: p.pubkey_hex,
                witness_script_hex: p.witness_script_hex,
            })
            .collect())
    }

    /// The preimage (hex) revealed by a redeem in `tx_hex`, if it opens this
    /// swap's hashlock.
    pub fn preimage_from_tx(&self, tx_hex: String) -> Option<String> {
        let hashlock = self.lock().hashlock?;
        let tx: Transaction = deserialize_hex(&tx_hex).ok()?;
        tx.input
            .iter()
            .find_map(|i| preimage_from_witness(&i.witness, &hashlock))
            .map(hex::encode)
    }

    /// Feed a chain event; returns the new state. Both are JSON tagged by
    /// `type`, e.g. `{"type":"CounterpartyFunded","confirmations":2}` →
    /// `{"type":"CounterpartyFundingConfirmed"}`.
    pub fn on_event(&self, event_json: String) -> Result<String> {
        let event: EventJson =
            serde_json::from_str(&event_json).map_err(|e| err(format!("swap event: {e}")))?;
        let mut s = self.lock();
        let hashlock = s.hashlock;
        let event = event.into_core(hashlock)?;
        let state = StateJson::from(machine(&mut s)?.on_event(event));
        Ok(serde_json::to_string(&state).expect("swap state serialises"))
    }
}

impl SwapSession {
    fn lock(&self) -> std::sync::MutexGuard<'_, Persisted> {
        self.inner.lock().unwrap()
    }

    fn our_preimage(&self) -> Option<[u8; 32]> {
        let nonce = self.nonce?;
        (self.role == SwapRole::Initiator).then(|| self.xbt_key.preimage(&self.match_id, &nonce))
    }

    fn send_params(&self) -> &ChainParams {
        match self.role {
            SwapRole::Initiator => &self.xbt_params,
            SwapRole::Participant => &self.btc_params,
        }
    }

    fn recv_params(&self) -> &ChainParams {
        match self.role {
            SwapRole::Initiator => &self.btc_params,
            SwapRole::Participant => &self.xbt_params,
        }
    }

    fn send_key(&self) -> &SwapKey {
        match self.role {
            SwapRole::Initiator => &self.xbt_key,
            SwapRole::Participant => &self.btc_key,
        }
    }

    fn recv_key(&self) -> &SwapKey {
        match self.role {
            SwapRole::Initiator => &self.btc_key,
            SwapRole::Participant => &self.xbt_key,
        }
    }

    fn address_spk(&self, address: &str, params: &ChainParams) -> Result<ScriptBuf> {
        Ok(parse_address(address, params.network)?.script_pubkey())
    }
}

fn machine(s: &mut Persisted) -> Result<&mut SwapMachine> {
    s.machine.as_mut().ok_or_else(|| err("counterparty not set yet"))
}

fn swap_key(params: &ChainParams, secret: Vec<u8>) -> Result<SwapKey> {
    let secret: [u8; 32] = secret.try_into().map_err(|_| err("swap secret must be 32 bytes"))?;
    Ok(SwapKey::from_secret(params, secret)?)
}

fn pubkey(hex_str: &str) -> Result<PublicKey> {
    hex::decode(hex_str)
        .ok()
        .and_then(|b| PublicKey::from_slice(&b).ok())
        .ok_or_else(|| err(format!("bad pubkey {hex_str}")))
}

fn outpoint(txid: &str, vout: u32) -> Result<OutPoint> {
    Ok(OutPoint::new(Txid::from_str(txid).map_err(|_| err(format!("bad txid {txid}")))?, vout))
}

fn htlc_info(c: &HtlcContract, params: &ChainParams) -> HtlcInfo {
    HtlcInfo {
        address: Address::from_script(&c.script_pubkey, params.network)
            .expect("P2WSH always has an address")
            .to_string(),
        amount_sat: c.value.to_sat(),
        script_pubkey_hex: hex::encode(c.script_pubkey.as_bytes()),
    }
}

/// `SwapEvent`, tagged by `type`, with hex txids and preimages.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum EventJson {
    OurFundingBroadcast { txid: String },
    CounterpartyFunded { confirmations: u32 },
    CounterpartyRedeemed { preimage: String },
    OurOutputConfirmed { confirmations: u32 },
    TimelockExpired,
}

impl EventJson {
    fn into_core(self, hashlock: Option<sha256::Hash>) -> Result<SwapEvent> {
        Ok(match self {
            EventJson::OurFundingBroadcast { txid } => SwapEvent::OurFundingBroadcast {
                txid: Txid::from_str(&txid).map_err(|_| err(format!("bad txid {txid}")))?,
            },
            EventJson::CounterpartyFunded { confirmations } => SwapEvent::CounterpartyFunded { confirmations },
            EventJson::CounterpartyRedeemed { preimage } => {
                let preimage: [u8; 32] = hex::decode(&preimage)
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| err("preimage must be 32 bytes of hex"))?;
                if Some(sha256::Hash::hash(&preimage)) != hashlock {
                    return Err(err("preimage does not match hashlock"));
                }
                SwapEvent::CounterpartyRedeemed { preimage }
            }
            EventJson::OurOutputConfirmed { confirmations } => SwapEvent::OurOutputConfirmed { confirmations },
            EventJson::TimelockExpired => SwapEvent::TimelockExpired,
        })
    }
}

/// `SwapState`, tagged by `type`, with hex txids and preimages.
#[derive(Serialize)]
#[serde(tag = "type")]
enum StateJson {
    Negotiated,
    OurFundingBroadcast { txid: String },
    CounterpartyFundingConfirmed,
    Redeemed { preimage: String },
    Settled,
    Refunded,
    Failed { reason: String },
}

impl From<&SwapState> for StateJson {
    fn from(s: &SwapState) -> Self {
        match s {
            SwapState::Negotiated => StateJson::Negotiated,
            SwapState::OurFundingBroadcast { txid } => StateJson::OurFundingBroadcast { txid: txid.to_string() },
            SwapState::CounterpartyFundingConfirmed => StateJson::CounterpartyFundingConfirmed,
            SwapState::Redeemed { preimage } => StateJson::Redeemed { preimage: hex::encode(preimage) },
            SwapState::Settled => StateJson::Settled,
            SwapState::Refunded => StateJson::Refunded,
            SwapState::Failed { reason } => StateJson::Failed { reason: reason.clone() },
        }
    }
}
