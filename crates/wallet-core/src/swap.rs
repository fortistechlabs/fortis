//! Client side of the HTLC atomic-swap protocol.
//!
//! The XBT holder always [`SwapRole::Initiator`]s, so the preimage is revealed by a
//! redeem on Bitcoin — the chain where a reversal is hardest.

use bitcoin::consensus::encode::{deserialize_hex, serialize_hex};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, Txid};
use serde::{Deserialize, Serialize};

use crate::chain::{Chain, ChainParams};
use crate::error::{Result, WalletError};
use crate::htlc::{htlc_spend_fee, redeem_witness, script_hashlock, HtlcContract, HtlcPath, CONTRACT_V1};
use crate::keys::SwapKey;
use crate::sighash::SighashVariant;
use crate::swap_terms::SwapTerms;
use crate::types::Preimage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapRole {
    /// Funds first; knows the preimage; reveals it by redeeming on the receive chain.
    Initiator,
    /// Funds second; redeems with the revealed preimage.
    Participant,
}

/// Terms both wallets agree (via the platform's matching + negotiation) before
/// anyone funds. A swap needs a keypair per party per chain, hence four pubkeys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapParams {
    pub swap_id: [u8; 16],
    pub role: SwapRole,
    /// Chain we send on (we fund an HTLC here).
    pub send_chain: Chain,
    /// Chain we receive on (the counterparty funds an HTLC here).
    pub recv_chain: Chain,
    pub send_amount: Amount,
    pub recv_amount: Amount,
    pub hashlock: sha256::Hash,

    pub our_pubkey_send: PublicKey,
    pub their_pubkey_send: PublicKey,
    pub our_pubkey_recv: PublicKey,
    pub their_pubkey_recv: PublicKey,

    /// Absolute CLTV (unix secs) on our contract (send chain).
    pub our_contract_locktime: u32,
    /// Absolute CLTV we expect on their contract (recv chain).
    pub their_contract_locktime: u32,

    /// Confirmations we require on the counterparty's funding tx before we fund.
    /// Deep on the BLAKE2b leg — see the reorg policy.
    pub required_incoming_confs: u32,

    /// Both legs' HTLC script version (from the terms). Sessions saved before
    /// versions existed hold v1 swaps.
    #[serde(default = "contract_v1")]
    pub contract_version: u8,
}

fn contract_v1() -> u8 {
    CONTRACT_V1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapState {
    Negotiated,
    /// We have broadcast our funding tx.
    OurFundingBroadcast { txid: Txid },
    /// Counterparty funding seen and buried `required_incoming_confs` deep.
    CounterpartyFundingConfirmed,
    /// The preimage is now public on-chain.
    Redeemed { preimage: Preimage },
    /// We hold the received coins with the agreed confirmations.
    Settled,
    /// Timelock hit; our funding was refunded.
    Refunded,
    Failed { reason: String },
}

/// An unsigned spend plus the sighash the shell must sign with the swap key.
#[derive(Debug, Clone)]
pub struct UnsignedSpend {
    pub tx: Transaction,
    pub sighash: [u8; 32],
}

/// Chain events the shell feeds in to advance the machine.
#[derive(Debug, Clone)]
pub enum SwapEvent {
    OurFundingBroadcast { txid: Txid },
    CounterpartyFunded { confirmations: u32 },
    CounterpartyRedeemed { preimage: Preimage },
    OurOutputConfirmed { confirmations: u32 },
    TimelockExpired,
}

/// One wallet's half of a swap. Serialisable so a swap engine can persist it
/// between ticks and across process restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapMachine {
    params: SwapParams,
    state: SwapState,
    our_contract: Option<HtlcContract>,
    their_contract: Option<HtlcContract>,
    /// Where our funding output landed (send chain), once known.
    #[serde(default)]
    our_outpoint: Option<OutPoint>,
    /// Where the counterparty's funding output landed (recv chain), once known.
    #[serde(default)]
    their_outpoint: Option<OutPoint>,
}

impl SwapMachine {
    pub fn new(params: SwapParams) -> Self {
        Self {
            params,
            state: SwapState::Negotiated,
            our_contract: None,
            their_contract: None,
            our_outpoint: None,
            their_outpoint: None,
        }
    }

    pub fn state(&self) -> &SwapState {
        &self.state
    }

    pub fn role(&self) -> SwapRole {
        self.params.role
    }

    /// Our HTLC on the send chain: redeemable by the counterparty with the preimage,
    /// refundable to us after `our_contract_locktime`.
    pub fn build_our_contract(&mut self, send_params: &ChainParams) -> Result<&HtlcContract> {
        let c = HtlcContract::try_build(
            send_params,
            self.params.contract_version,
            self.params.hashlock,
            &self.params.their_pubkey_send,
            &self.params.our_pubkey_send,
            self.params.our_contract_locktime,
            self.params.send_amount,
        )?;
        Ok(self.our_contract.insert(c))
    }

    /// The contract we expect the counterparty to have funded on the receive chain.
    pub fn expected_their_contract(&self, recv_params: &ChainParams) -> HtlcContract {
        HtlcContract::build(
            recv_params,
            self.params.contract_version,
            self.params.hashlock,
            &self.params.our_pubkey_recv,
            &self.params.their_pubkey_recv,
            self.params.their_contract_locktime,
            self.params.recv_amount,
        )
    }

    /// Verify the counterparty's on-chain funding transaction contains an output that
    /// exactly matches the agreed HTLC with exactly the agreed amount. Returns the outpoint.
    pub fn verify_their_contract(
        &mut self,
        funding_tx: &[u8],
        recv_params: &ChainParams,
    ) -> Result<OutPoint> {
        let expected = self.expected_their_contract(recv_params);
        let tx: Transaction = bitcoin::consensus::deserialize(funding_tx)
            .map_err(|e| WalletError::Bitcoin(format!("funding tx decode: {e}")))?;

        let vout = expected
            .find_exact_output(&tx, self.params.recv_amount)
            .ok_or(WalletError::ContractMismatch)?;

        self.their_contract = Some(expected);
        Ok(OutPoint { txid: tx.compute_txid(), vout })
    }

    /// Verify one counterparty funding output (as a chain backend reports it) pays
    /// exactly the agreed HTLC's P2WSH with exactly the agreed amount (over-funding is
    /// refused: redeem signing commits to `recv_amount`). On success
    /// the contract is stored, as [`Self::verify_their_contract`] does.
    pub fn verify_their_output(
        &mut self,
        script_pubkey: &ScriptBuf,
        value: Amount,
        recv_params: &ChainParams,
    ) -> Result<()> {
        let expected = self.expected_their_contract(recv_params);
        if *script_pubkey != expected.script_pubkey || value != self.params.recv_amount {
            return Err(WalletError::ContractMismatch);
        }
        self.their_contract = Some(expected);
        Ok(())
    }

    pub fn our_outpoint(&self) -> Option<OutPoint> {
        self.our_outpoint
    }

    pub fn set_our_outpoint(&mut self, outpoint: OutPoint) {
        self.our_outpoint = Some(outpoint);
    }

    pub fn their_outpoint(&self) -> Option<OutPoint> {
        self.their_outpoint
    }

    pub fn set_their_outpoint(&mut self, outpoint: OutPoint) {
        self.their_outpoint = Some(outpoint);
    }

    /// Unsigned refund of our own contract (send chain), spendable after our CLTV.
    pub fn build_refund(
        &self,
        our_contract_outpoint: OutPoint,
        our_payout_spk: ScriptBuf,
        fee: Amount,
    ) -> Result<UnsignedSpend> {
        let c = self.our_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "our contract built".into(),
            actual: "none".into(),
        })?;
        let tx = c.refund_tx(our_contract_outpoint, our_payout_spk, fee)?;
        let sighash = c.spend_sighash(&tx, 0)?;
        Ok(UnsignedSpend { tx, sighash })
    }

    /// Unsigned redeem of the counterparty's contract (recv chain) using the preimage.
    pub fn build_redeem(
        &self,
        their_contract_outpoint: OutPoint,
        our_payout_spk: ScriptBuf,
        fee: Amount,
    ) -> Result<UnsignedSpend> {
        let c = self.their_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "their contract verified".into(),
            actual: "none".into(),
        })?;
        let tx = c.redeem_tx(their_contract_outpoint, our_payout_spk, fee)?;
        let sighash = c.spend_sighash(&tx, 0)?;
        Ok(UnsignedSpend { tx, sighash })
    }

    /// Attach the witness to our refund transaction (spends our own contract via the
    /// timelock path). `sig` is the output of `MasterKey::sign_swap`.
    pub fn finalize_our_refund(
        &self,
        tx: &mut Transaction,
        sig: &[u8],
        refund_pubkey: &PublicKey,
    ) -> Result<()> {
        let c = self.our_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "our contract built".into(),
            actual: "none".into(),
        })?;
        c.finalize_refund(tx, 0, sig, refund_pubkey);
        Ok(())
    }

    /// Attach the witness to our redeem of the counterparty's contract (hashlock
    /// path, using the revealed preimage).
    pub fn finalize_their_redeem(
        &self,
        tx: &mut Transaction,
        sig: &[u8],
        redeem_pubkey: &PublicKey,
        preimage: &Preimage,
    ) -> Result<()> {
        let c = self.their_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "their contract verified".into(),
            actual: "none".into(),
        })?;
        c.finalize_redeem(tx, 0, sig, redeem_pubkey, preimage);
        Ok(())
    }

    /// Fully signed refunds of our own contract, one per feerate (cheapest first,
    /// as given), so the engine can broadcast the low one and bump if it stalls.
    pub fn signed_refund(
        &self,
        outpoint: OutPoint,
        to: ScriptBuf,
        key: &SwapKey,
        feerates: &[u64],
    ) -> Result<Vec<Transaction>> {
        let c = self.our_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "our contract built".into(),
            actual: "none".into(),
        })?;
        check_key(key, &self.params.our_pubkey_send, c)?;
        spend_fees(c, HtlcPath::Refund, &to, feerates)?
            .into_iter()
            .map(|fee| {
                let mut tx = c.refund_tx(outpoint, to.clone(), fee)?;
                let sig = key.sign(&c.spend_sighash(&tx, 0)?);
                c.finalize_refund(&mut tx, 0, &sig, &key.pubkey());
                Ok(tx)
            })
            .collect()
    }

    /// Fully signed redeems of the counterparty's contract, one per feerate.
    pub fn signed_redeem(
        &self,
        outpoint: OutPoint,
        to: ScriptBuf,
        key: &SwapKey,
        preimage: &Preimage,
        feerates: &[u64],
    ) -> Result<Vec<Transaction>> {
        if sha256::Hash::hash(preimage) != self.params.hashlock {
            return Err(WalletError::InvalidInput("preimage does not match hashlock".into()));
        }
        self.presign_redeem(outpoint, to, key, feerates)?
            .iter()
            .map(|p| finish_presigned_redeem(p, preimage))
            .collect()
    }

    /// Releases of our contract (v2) at `feerates`, unsigned, paying `to`
    /// (our refund address): the counterparty signs one when the match is
    /// cancelled before it funded, and [`Self::finish_release`] completes it.
    pub fn release_txs(&self, outpoint: OutPoint, to: ScriptBuf, feerates: &[u64]) -> Result<Vec<Transaction>> {
        let c = self.our_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "our contract built".into(),
            actual: "none".into(),
        })?;
        if !c.has_release() {
            return Err(WalletError::InvalidSwapParams("this swap's contract has no release".into()));
        }
        spend_fees(c, HtlcPath::Release, &to, feerates)?
            .into_iter()
            .map(|fee| c.release_tx(outpoint, to.clone(), fee))
            .collect()
    }

    /// Our signature (as the redeem party) on `tx`, a release of the
    /// counterparty's contract at `outpoint` back to them: we give up claiming
    /// it. Only a single-input spend of that very outpoint is signed — the
    /// signature then moves nothing else. The caller must never fund this swap
    /// afterwards: with the release, the counterparty has its coins back.
    pub fn sign_their_release(
        &self,
        tx: &Transaction,
        outpoint: OutPoint,
        key: &SwapKey,
        recv_params: &ChainParams,
    ) -> Result<Vec<u8>> {
        let c = self.expected_their_contract(recv_params);
        if !c.has_release() {
            return Err(WalletError::InvalidSwapParams("this swap's contract has no release".into()));
        }
        check_key(key, &self.params.our_pubkey_recv, &c)?;
        let [input] = tx.input.as_slice() else {
            return Err(WalletError::InvalidInput("a release spends exactly one input".into()));
        };
        if input.previous_output != outpoint {
            return Err(WalletError::InvalidInput("the release doesn't spend their contract".into()));
        }
        Ok(key.sign(&c.spend_sighash(tx, 0)?))
    }

    /// Complete `tx`, one of our [`Self::release_txs`], with the counterparty's
    /// signature `their_sig` and ours. Refused unless it spends exactly our
    /// contract at `outpoint`, pays `to` at no more than the highest release
    /// fee, and `their_sig` verifies.
    pub fn finish_release(
        &self,
        tx: &Transaction,
        outpoint: OutPoint,
        to: &ScriptBuf,
        their_sig: &[u8],
        key: &SwapKey,
    ) -> Result<Transaction> {
        let c = self.our_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "our contract built".into(),
            actual: "none".into(),
        })?;
        check_key(key, &self.params.our_pubkey_send, c)?;
        let max_fee = htlc_spend_fee(c, HtlcPath::Release, MAX_SWAP_FEERATE_SAT_VB);
        let ours = c.release_tx(outpoint, to.clone(), Amount::ZERO)?;
        let paid = tx.output.first().map(|o| o.value).unwrap_or(Amount::ZERO);
        let same_shape = tx.input.len() == 1
            && tx.input[0].previous_output == outpoint
            && tx.lock_time == ours.lock_time
            && tx.input[0].sequence == ours.input[0].sequence
            && tx.output.len() == ours.output.len()
            && tx.output[0].script_pubkey == *to
            && tx.output[1..] == ours.output[1..];
        if !same_shape || paid + max_fee < c.value {
            return Err(WalletError::InvalidInput("not our release".into()));
        }
        let msg = c.spend_sighash(tx, 0)?;
        let flag = key.flag();
        let verified = their_sig.last() == Some(&flag)
            && bitcoin::secp256k1::ecdsa::Signature::from_der(&their_sig[..their_sig.len() - 1])
                .ok()
                .is_some_and(|s| {
                    bitcoin::secp256k1::Secp256k1::verification_only()
                        .verify_ecdsa(&bitcoin::secp256k1::Message::from_digest(msg), &s, &self.params.their_pubkey_send)
                        .is_ok()
                });
        if !verified {
            return Err(WalletError::InvalidInput("the counterparty's release signature doesn't verify".into()));
        }
        let mut done = tx.clone();
        c.finalize_release(&mut done, 0, their_sig, &key.sign(&msg))?;
        Ok(done)
    }

    /// Redeems of the counterparty's contract signed now, before the preimage is
    /// known; [`finish_presigned_redeem`] completes one once the preimage appears
    /// on chain. Lets the participant redeem without the seed being unlocked.
    pub fn presign_redeem(
        &self,
        outpoint: OutPoint,
        to: ScriptBuf,
        key: &SwapKey,
        feerates: &[u64],
    ) -> Result<Vec<PresignedRedeem>> {
        let c = self.their_contract.as_ref().ok_or(WalletError::SwapState {
            expected: "their contract verified".into(),
            actual: "none".into(),
        })?;
        check_key(key, &self.params.our_pubkey_recv, c)?;
        spend_fees(c, HtlcPath::Redeem, &to, feerates)?
            .into_iter()
            .map(|fee| {
                let tx = c.redeem_tx(outpoint, to.clone(), fee)?;
                let sig = key.sign(&c.spend_sighash(&tx, 0)?);
                Ok(PresignedRedeem {
                    tx_hex: serialize_hex(&tx),
                    sig_hex: hex::encode(sig),
                    pubkey_hex: hex::encode(key.pubkey().serialize()),
                    witness_script_hex: hex::encode(c.witness_script.as_bytes()),
                })
            })
            .collect()
    }

    /// Advance on a chain event. Transitions are intentionally conservative — an
    /// unexpected event is recorded rather than acted on.
    pub fn on_event(&mut self, event: SwapEvent) -> &SwapState {
        self.state = match (&self.state, event) {
            (SwapState::Negotiated, SwapEvent::OurFundingBroadcast { txid }) => {
                SwapState::OurFundingBroadcast { txid }
            }
            (_, SwapEvent::CounterpartyFunded { confirmations })
                if confirmations >= self.params.required_incoming_confs =>
            {
                SwapState::CounterpartyFundingConfirmed
            }
            (_, SwapEvent::CounterpartyRedeemed { preimage }) => SwapState::Redeemed { preimage },
            (SwapState::Redeemed { .. }, SwapEvent::OurOutputConfirmed { confirmations })
                if confirmations >= self.params.required_incoming_confs =>
            {
                SwapState::Settled
            }
            (SwapState::OurFundingBroadcast { .. }, SwapEvent::TimelockExpired) => {
                SwapState::Refunded
            }
            (other, _) => other.clone(),
        };
        &self.state
    }
}

/// A redeem signed ahead of time, missing only the preimage. Plain hex so it can
/// be stored next to the machine and finished without the swap key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedRedeem {
    /// The unsigned, witness-less redeem transaction.
    pub tx_hex: String,
    /// DER signature plus sighash flag byte.
    pub sig_hex: String,
    pub pubkey_hex: String,
    pub witness_script_hex: String,
}

/// Attach the redeem witness to a [`PresignedRedeem`] once the preimage is known.
/// Refuses a preimage that does not open the script's hashlock.
pub fn finish_presigned_redeem(p: &PresignedRedeem, preimage: &Preimage) -> Result<Transaction> {
    let bad = |what: &str| WalletError::InvalidInput(format!("presigned redeem: bad {what}"));
    let mut tx: Transaction = deserialize_hex(&p.tx_hex).map_err(|_| bad("tx"))?;
    let sig = hex::decode(&p.sig_hex).map_err(|_| bad("signature"))?;
    let pubkey = hex::decode(&p.pubkey_hex)
        .ok()
        .and_then(|b| PublicKey::from_slice(&b).ok())
        .ok_or_else(|| bad("pubkey"))?;
    let ws = ScriptBuf::from(hex::decode(&p.witness_script_hex).map_err(|_| bad("witness script"))?);
    if tx.input.len() != 1 {
        return Err(bad("tx (expected one input)"));
    }
    if script_hashlock(&ws) != Some(sha256::Hash::hash(preimage)) {
        return Err(WalletError::InvalidInput("preimage does not match hashlock".into()));
    }
    tx.input[0].witness = redeem_witness(&sig, &pubkey, preimage, &ws);
    Ok(tx)
}

/// `key` must be the one the contract expects and sign with that chain's sighash flag.
fn check_key(key: &SwapKey, expected: &PublicKey, c: &HtlcContract) -> Result<()> {
    let flag = match c.sighash_variant {
        SighashVariant::SegwitV0 => 0x01,
        SighashVariant::Unified => 0x21,
    };
    if key.pubkey() != *expected || key.flag() != flag {
        return Err(WalletError::InvalidSwapParams("swap key does not match contract".into()));
    }
    Ok(())
}

/// The highest feerate (sat/vB) a swap claim or refund is signed at. Feerates
/// come from the edge; one above this (a hostile or broken edge) is clamped to
/// it, so a spend can't be turned into a gift to miners. Well above any
/// sustained BTC fee level, and far above XBT's.
pub const MAX_SWAP_FEERATE_SAT_VB: u64 = 500;

/// The fee of each spend variant, one per feerate (cheapest first, as given). A fee
/// that would leave less than `to`'s dust limit is clamped to `value - dust`, so on
/// a small leg the top variant becomes "everything but dust" rather than an error;
/// variants made identical by the clamp are dropped (order kept). Refund, redeem and
/// presigned redeem all use this list, so presigned sigs and finished txs match.
fn spend_fees(c: &HtlcContract, path: HtlcPath, to: &ScriptBuf, feerates: &[u64]) -> Result<Vec<Amount>> {
    if feerates.is_empty() {
        return Err(WalletError::InvalidInput("no feerates given".into()));
    }
    let dust = to.minimal_non_dust();
    let mut fees: Vec<Amount> = Vec::with_capacity(feerates.len());
    for &rate in feerates {
        let fee = htlc_spend_fee(c, path, rate.min(MAX_SWAP_FEERATE_SAT_VB));
        let max_fee = c.value.checked_sub(dust).filter(|m| *m > Amount::ZERO).ok_or(
            WalletError::InsufficientFunds { need: (fee + dust).to_sat(), have: c.value.to_sat() },
        )?;
        let fee = fee.min(max_fee);
        if !fees.contains(&fee) {
            fees.push(fee);
        }
    }
    Ok(fees)
}

/// Swap parameters for our side of `t`. `ours` / `theirs` are `(xbt_pubkey,
/// btc_pubkey)`. The initiator sends XBT and the participant BTC, so both phones
/// build byte-identical contracts from the same terms.
pub fn params_from_terms(
    t: &SwapTerms,
    role: SwapRole,
    ours: (PublicKey, PublicKey),
    theirs: (PublicKey, PublicKey),
    hashlock: sha256::Hash,
    swap_id: [u8; 16],
) -> SwapParams {
    let (xbt, btc) = (Amount::from_sat(t.xbt_sat), Amount::from_sat(t.btc_sat));
    let ((our_xbt, our_btc), (their_xbt, their_btc)) = (ours, theirs);
    match role {
        SwapRole::Initiator => SwapParams {
            swap_id,
            role,
            send_chain: Chain::Xbt,
            recv_chain: Chain::Btc,
            send_amount: xbt,
            recv_amount: btc,
            hashlock,
            our_pubkey_send: our_xbt,
            their_pubkey_send: their_xbt,
            our_pubkey_recv: our_btc,
            their_pubkey_recv: their_btc,
            our_contract_locktime: t.xbt_locktime,
            their_contract_locktime: t.btc_locktime,
            required_incoming_confs: t.btc_confs,
            contract_version: t.contract_version,
        },
        SwapRole::Participant => SwapParams {
            swap_id,
            role,
            send_chain: Chain::Btc,
            recv_chain: Chain::Xbt,
            send_amount: btc,
            recv_amount: xbt,
            hashlock,
            our_pubkey_send: our_btc,
            their_pubkey_send: their_btc,
            our_pubkey_recv: our_xbt,
            their_pubkey_recv: their_xbt,
            our_contract_locktime: t.btc_locktime,
            their_contract_locktime: t.xbt_locktime,
            required_incoming_confs: t.xbt_confs,
            contract_version: t.contract_version,
        },
    }
}

/// Relative lifetimes of the two HTLC legs. The XBT leg (funded first by the
/// initiator) is always the longer one, so the initiator cannot refund and still
/// redeem. Order-free: both wallets derive the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapTimelocks {
    pub xbt_leg_secs: u32,
    pub btc_leg_secs: u32,
}

pub fn swap_timelocks(btc: &ChainParams, xbt: &ChainParams) -> SwapTimelocks {
    SwapTimelocks {
        xbt_leg_secs: xbt.htlc_initiator_locktime_secs,
        btc_leg_secs: btc.htlc_participant_locktime_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::consensus::encode::serialize;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bitcoin::{transaction, TxOut};

    fn pk(b: u8) -> PublicKey {
        let secp = Secp256k1::new();
        PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[b; 32]).unwrap())
    }

    fn params() -> SwapParams {
        SwapParams {
            swap_id: [0u8; 16],
            role: SwapRole::Participant,
            send_chain: Chain::Btc,
            recv_chain: Chain::Xbt,
            send_amount: Amount::from_sat(1_000_000),
            recv_amount: Amount::from_sat(2_000_000),
            hashlock: sha256::Hash::hash(b"x"),
            our_pubkey_send: pk(1),
            their_pubkey_send: pk(2),
            our_pubkey_recv: pk(3),
            their_pubkey_recv: pk(4),
            our_contract_locktime: 1_800_000_000,
            their_contract_locktime: 1_800_100_000,
            required_incoming_confs: 3,
            contract_version: crate::htlc::CURRENT_CONTRACT,
        }
    }

    #[test]
    fn verify_their_contract_matches_expected_output() {
        let mut m = SwapMachine::new(params());
        let xbt = ChainParams::blake2b();
        let expected = m.expected_their_contract(&xbt);

        let funding = Transaction {
            version: transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![
                TxOut { value: Amount::from_sat(10), script_pubkey: ScriptBuf::new() },
                TxOut { value: Amount::from_sat(2_000_000), script_pubkey: expected.script_pubkey.clone() },
            ],
        };
        let op = m.verify_their_contract(&serialize(&funding), &xbt).unwrap();
        assert_eq!(op.vout, 1);
        assert!(m.their_contract.is_some());
    }

    #[test]
    fn verify_rejects_short_funding() {
        let mut m = SwapMachine::new(params());
        let xbt = ChainParams::blake2b();
        let expected = m.expected_their_contract(&xbt);
        let funding = Transaction {
            version: transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1_999_999),
                script_pubkey: expected.script_pubkey,
            }],
        };
        assert!(m.verify_their_contract(&serialize(&funding), &xbt).is_err());
    }

    #[test]
    fn verify_their_output_takes_the_exact_script_and_amount() {
        let xbt = ChainParams::blake2b();
        let mut m = SwapMachine::new(params());
        let expected = m.expected_their_contract(&xbt);
        m.verify_their_output(&expected.script_pubkey, Amount::from_sat(2_000_000), &xbt).unwrap();
        assert_eq!(m.their_contract.as_ref().unwrap().witness_script, expected.witness_script);
    }

    #[test]
    fn verify_their_output_rejects_one_sat_short() {
        let xbt = ChainParams::blake2b();
        let mut m = SwapMachine::new(params());
        let spk = m.expected_their_contract(&xbt).script_pubkey;
        assert!(matches!(
            m.verify_their_output(&spk, Amount::from_sat(1_999_999), &xbt),
            Err(WalletError::ContractMismatch)
        ));
        assert!(m.their_contract.is_none());
    }

    #[test]
    fn verify_their_output_rejects_overfunding() {
        let xbt = ChainParams::blake2b();
        let mut m = SwapMachine::new(params());
        let spk = m.expected_their_contract(&xbt).script_pubkey;
        assert!(matches!(
            m.verify_their_output(&spk, Amount::from_sat(2_000_001), &xbt),
            Err(WalletError::ContractMismatch)
        ));
        assert!(m.their_contract.is_none());
    }

    #[test]
    fn verify_their_contract_rejects_overfunding() {
        let xbt = ChainParams::blake2b();
        let mut m = SwapMachine::new(params());
        let expected = m.expected_their_contract(&xbt);
        let funding = Transaction {
            version: transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(2_000_001),
                script_pubkey: expected.script_pubkey,
            }],
        };
        assert!(m.verify_their_contract(&serialize(&funding), &xbt).is_err());
        assert!(m.their_contract.is_none());
    }

    #[test]
    fn verify_their_output_rejects_a_different_script() {
        let xbt = ChainParams::blake2b();
        let mut m = SwapMachine::new(params());
        // same parties, different locktime: a different P2WSH
        let mut p = params();
        p.their_contract_locktime += 1;
        let other = SwapMachine::new(p).expected_their_contract(&xbt).script_pubkey;
        assert!(matches!(
            m.verify_their_output(&other, Amount::from_sat(2_000_000), &xbt),
            Err(WalletError::ContractMismatch)
        ));
        assert!(m.their_contract.is_none());
    }

    #[test]
    fn outpoints_survive_serde() {
        let mut m = SwapMachine::new(params());
        assert_eq!((m.our_outpoint(), m.their_outpoint()), (None, None));
        let (ours, theirs) = (OutPoint::new(Txid::all_zeros(), 1), OutPoint::new(Txid::all_zeros(), 2));
        m.set_our_outpoint(ours);
        m.set_their_outpoint(theirs);
        let back: SwapMachine = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!((back.our_outpoint(), back.their_outpoint()), (Some(ours), Some(theirs)));
    }

    use crate::htlc::preimage_from_witness;
    use crate::keys::{MasterKey, SwapKey};
    use crate::swap_terms::{btc_for_xbt, network_params, SwapTerms};

    const MATCHED_AT: u32 = 1_800_000_000;

    fn terms() -> SwapTerms {
        let price = 1_120_000;
        SwapTerms {
            match_id: "m1".into(),
            network: "bitcoin".into(),
            xbt_sat: 10_000_000,
            btc_sat: btc_for_xbt(10_000_000, price),
            price_sat_per_xbt: price,
            fee_bps: 50,
            fee_address: "bc1qfee".into(),
            matched_at: MATCHED_AT,
            xbt_locktime: MATCHED_AT + 48 * 3600,
            btc_locktime: MATCHED_AT + 24 * 3600,
            xbt_confs: 6,
            btc_confs: 2,
            accept_by: MATCHED_AT + 120,
            initiator_fund_by: MATCHED_AT + 1920,
            contract_version: crate::htlc::CURRENT_CONTRACT,
        }
    }

    struct Party {
        xbt: SwapKey,
        btc: SwapKey,
    }

    impl Party {
        fn new(seed: u8) -> Self {
            let (_m, mk) = MasterKey::generate(&[seed; 32]).unwrap();
            let (btc, xbt) = network_params("bitcoin").unwrap();
            Party {
                xbt: SwapKey::from_secret(&xbt, mk.swap_secret(&xbt, 0, 3).unwrap()).unwrap(),
                btc: SwapKey::from_secret(&btc, mk.swap_secret(&btc, 0, 3).unwrap()).unwrap(),
            }
        }
        fn pks(&self) -> (PublicKey, PublicKey) {
            (self.xbt.pubkey(), self.btc.pubkey())
        }
    }

    /// (alice = initiator, bob = participant, their machines, the preimage)
    fn pair() -> (Party, Party, SwapMachine, SwapMachine, Preimage) {
        let (alice, bob) = (Party::new(0xa1), Party::new(0xb0));
        let preimage = alice.xbt.preimage("m1", &[1u8; 16]);
        let hashlock = sha256::Hash::hash(&preimage);
        let t = terms();
        let am = SwapMachine::new(params_from_terms(
            &t, SwapRole::Initiator, alice.pks(), bob.pks(), hashlock, [1; 16],
        ));
        let bm = SwapMachine::new(params_from_terms(
            &t, SwapRole::Participant, bob.pks(), alice.pks(), hashlock, [1; 16],
        ));
        (alice, bob, am, bm, preimage)
    }

    fn funding_paying(c: &HtlcContract) -> Transaction {
        Transaction {
            version: transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![c.funding_output()],
        }
    }

    fn fee_of(c: &HtlcContract, tx: &Transaction) -> u64 {
        c.value.to_sat() - tx.output[0].value.to_sat()
    }

    #[test]
    fn both_roles_build_the_same_contracts() {
        let (btc, xbt) = network_params("bitcoin").unwrap();
        let (_a, _b, mut am, mut bm, _p) = pair();
        assert_eq!(am.params.send_chain, Chain::Xbt);
        assert_eq!(bm.params.send_chain, Chain::Btc);
        assert_eq!(am.params.required_incoming_confs, 2);
        assert_eq!(bm.params.required_incoming_confs, 6);

        let a_xbt = am.build_our_contract(&xbt).unwrap().clone();
        assert_eq!(a_xbt.witness_script, bm.expected_their_contract(&xbt).witness_script);
        assert_eq!(a_xbt.locktime, MATCHED_AT + 48 * 3600);
        assert_eq!(a_xbt.value, Amount::from_sat(10_000_000));

        let b_btc = bm.build_our_contract(&btc).unwrap().clone();
        assert_eq!(b_btc.witness_script, am.expected_their_contract(&btc).witness_script);
        assert_eq!(b_btc.locktime, MATCHED_AT + 24 * 3600);
        assert_eq!(b_btc.value, Amount::from_sat(112_000));

        let op = bm.verify_their_contract(&serialize(&funding_paying(&a_xbt)), &xbt).unwrap();
        assert_eq!(op.vout, 0);
        am.verify_their_contract(&serialize(&funding_paying(&b_btc)), &btc).unwrap();
    }

    #[test]
    fn refund_and_redeem_come_at_two_feerates() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, _b, mut am, _bm, _p) = pair();
        let c = am.build_our_contract(&xbt).unwrap().clone();
        let op = OutPoint::new(Txid::all_zeros(), 0);
        let to = ScriptBuf::from(vec![0u8; 22]);

        let txs = am.signed_refund(op, to.clone(), &alice.xbt, &[2, 20]).unwrap();
        assert_eq!(txs.len(), 2);
        assert!(fee_of(&c, &txs[1]) > fee_of(&c, &txs[0]));
        assert_eq!(fee_of(&c, &txs[0]), htlc_spend_fee(&c, HtlcPath::Refund, 2).to_sat());
        for tx in &txs {
            assert_eq!(tx.input[0].witness.len(), 5, "a v2 refund: [sig, pubkey, 0x01, <empty>, ws]");
            assert_eq!(tx.lock_time, bitcoin::absolute::LockTime::from_consensus(c.locktime));
            assert_eq!(tx.input[0].sequence, bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME);
            assert_eq!(tx.input[0].witness.nth(0).unwrap().last(), Some(&0x21));
        }
        // the wrong key is refused rather than producing an unspendable refund
        assert!(am.signed_refund(op, to, &alice.btc, &[2]).is_err());
    }

    /// Feerates come from the edge: an absurd one (a hostile or broken edge)
    /// must not turn a claim or refund into "all but dust" to miners. Above
    /// the ceiling they are clamped to it — never refused, so a spend stays possible.
    #[test]
    fn swap_spend_feerates_are_capped() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, _b, mut am, _bm, _p) = pair();
        let c = am.build_our_contract(&xbt).unwrap().clone();
        let op = OutPoint::new(Txid::all_zeros(), 0);
        let to = ScriptBuf::from(vec![0u8; 22]);
        let txs = am.signed_refund(op, to, &alice.xbt, &[100_000]).unwrap();
        assert_eq!(fee_of(&c, &txs[0]), htlc_spend_fee(&c, HtlcPath::Refund, MAX_SWAP_FEERATE_SAT_VB).to_sat());
        assert!(MAX_SWAP_FEERATE_SAT_VB <= 1_000);
        // Two absurd rates collapse to one capped variant.
        let (alice, _b, mut am, _bm, _p) = pair();
        am.build_our_contract(&xbt).unwrap();
        let txs = am.signed_refund(op, ScriptBuf::from(vec![0u8; 22]), &alice.xbt, &[50_000, 100_000]).unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn presigned_redeem_finishes_later() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (_alice, bob, mut am, mut bm, preimage) = pair();
        let a_xbt = am.build_our_contract(&xbt).unwrap().clone();
        let op = bm.verify_their_contract(&serialize(&funding_paying(&a_xbt)), &xbt).unwrap();
        let to = ScriptBuf::from(vec![0u8; 22]);

        let pre = bm.presign_redeem(op, to.clone(), &bob.xbt, &[3, 30]).unwrap();
        assert_eq!(pre.len(), 2);
        let json = serde_json::to_string(&pre[0]).unwrap();
        let pre0: PresignedRedeem = serde_json::from_str(&json).unwrap();

        let done = finish_presigned_redeem(&pre0, &preimage).unwrap();
        let direct = bm.signed_redeem(op, to, &bob.xbt, &preimage, &[3]).unwrap();
        assert_eq!(done.input[0].witness, direct[0].input[0].witness);
        assert_eq!(done, direct[0]);
        let hashlock = <bitcoin::hashes::sha256::Hash as bitcoin::hashes::Hash>::hash(&preimage);
        assert_eq!(preimage_from_witness(&done.input[0].witness, &hashlock), Some(preimage));

        // a preimage that does not open the hashlock is refused
        assert!(finish_presigned_redeem(&pre0, &[0u8; 32]).is_err());
    }

    /// Machines over a `sat`-sat leg on each chain (alice = initiator, bob = participant).
    fn small_pair(sat: u64) -> (Party, Party, SwapMachine, SwapMachine, Preimage) {
        let (alice, bob) = (Party::new(0xa1), Party::new(0xb0));
        let preimage = alice.xbt.preimage("m1", &[1u8; 16]);
        let hashlock = sha256::Hash::hash(&preimage);
        let t = SwapTerms { xbt_sat: sat, btc_sat: sat, ..terms() };
        let am = SwapMachine::new(params_from_terms(
            &t, SwapRole::Initiator, alice.pks(), bob.pks(), hashlock, [1; 16],
        ));
        let bm = SwapMachine::new(params_from_terms(
            &t, SwapRole::Participant, bob.pks(), alice.pks(), hashlock, [1; 16],
        ));
        (alice, bob, am, bm, preimage)
    }

    fn p2wpkh() -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::all_zeros())
    }

    #[test]
    fn small_leg_spends_clamp_instead_of_failing() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, bob, mut am, mut bm, preimage) = small_pair(20_000);
        let c = am.build_our_contract(&xbt).unwrap().clone();
        let op = bm.verify_their_contract(&serialize(&funding_paying(&c)), &xbt).unwrap();
        let to = p2wpkh();
        let dust = to.minimal_non_dust();
        assert_eq!(dust.to_sat(), 294);

        let refunds = am.signed_refund(op, to.clone(), &alice.xbt, &[70, 140]).unwrap();
        let redeems = bm.signed_redeem(op, to.clone(), &bob.xbt, &preimage, &[70, 140]).unwrap();
        let presigned = bm.presign_redeem(op, to.clone(), &bob.xbt, &[70, 140]).unwrap();
        for txs in [&refunds, &redeems] {
            assert_eq!(txs.len(), 2);
            assert!(txs.iter().all(|tx| tx.output[0].value >= dust));
            assert_eq!(txs[1].output[0].value, dust);
            assert!(txs[0].output[0].value > dust);
        }
        assert_eq!(fee_of(&c, &refunds[0]), htlc_spend_fee(&c, HtlcPath::Refund, 70).to_sat());
        assert_eq!(fee_of(&c, &redeems[0]), htlc_spend_fee(&c, HtlcPath::Redeem, 70).to_sat());
        assert_eq!(presigned.len(), redeems.len());
        for (p, tx) in presigned.iter().zip(&redeems) {
            assert_eq!(&finish_presigned_redeem(p, &preimage).unwrap(), tx);
        }

        // a leg that cannot even leave a non-dust output is still refused
        let (alice, _bob, mut am, _bm, _p) = small_pair(294);
        am.build_our_contract(&xbt).unwrap();
        assert!(am.signed_refund(op, to, &alice.xbt, &[1]).is_err());
    }

    #[test]
    fn clamped_duplicates_are_dropped() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, bob, mut am, mut bm, preimage) = small_pair(20_000);
        let c = am.build_our_contract(&xbt).unwrap().clone();
        let op = bm.verify_their_contract(&serialize(&funding_paying(&c)), &xbt).unwrap();
        let to = p2wpkh();
        let dust = to.minimal_non_dust();

        let refunds = am.signed_refund(op, to.clone(), &alice.xbt, &[200, 400]).unwrap();
        let redeems = bm.signed_redeem(op, to.clone(), &bob.xbt, &preimage, &[200, 400]).unwrap();
        let presigned = bm.presign_redeem(op, to, &bob.xbt, &[200, 400]).unwrap();
        assert_eq!(refunds.len(), 1);
        assert_eq!(redeems.len(), 1);
        assert_eq!(presigned.len(), 1);
        assert_eq!(refunds[0].output[0].value, dust);
        assert_eq!(redeems[0].output[0].value, dust);
    }

    #[test]
    fn swap_machine_serde_round_trip() {
        let (btc, xbt) = network_params("bitcoin").unwrap();
        let (_a, _b, mut am, _bm, preimage) = pair();
        am.build_our_contract(&xbt).unwrap();
        let theirs = am.expected_their_contract(&btc);
        am.verify_their_contract(&serialize(&funding_paying(&theirs)), &btc).unwrap();
        am.on_event(SwapEvent::OurFundingBroadcast { txid: Txid::all_zeros() });
        am.on_event(SwapEvent::CounterpartyFunded { confirmations: 2 });
        am.on_event(SwapEvent::CounterpartyRedeemed { preimage });
        assert!(matches!(am.state(), SwapState::Redeemed { .. }));

        let json = serde_json::to_string(&am).unwrap();
        let back: SwapMachine = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state(), am.state());
        assert_eq!(
            back.our_contract.as_ref().unwrap().script_pubkey,
            am.our_contract.as_ref().unwrap().script_pubkey
        );
        assert_eq!(
            back.their_contract.as_ref().unwrap().witness_script,
            am.their_contract.as_ref().unwrap().witness_script
        );
        assert_eq!(back.our_contract.as_ref().unwrap().value, am.our_contract.as_ref().unwrap().value);
    }

    #[test]
    fn state_machine_happy_path() {
        let mut m = SwapMachine::new(params());
        m.on_event(SwapEvent::OurFundingBroadcast { txid: Txid::all_zeros() });
        assert!(matches!(m.state(), SwapState::OurFundingBroadcast { .. }));
        m.on_event(SwapEvent::CounterpartyFunded { confirmations: 5 });
        assert_eq!(m.state(), &SwapState::CounterpartyFundingConfirmed);
        m.on_event(SwapEvent::CounterpartyRedeemed { preimage: [9u8; 32] });
        assert!(matches!(m.state(), SwapState::Redeemed { .. }));
        m.on_event(SwapEvent::OurOutputConfirmed { confirmations: 3 });
        assert_eq!(m.state(), &SwapState::Settled);
    }

    // ---- the cooperative release (v2 contracts) ----

    fn verifies(sig: &[u8], msg: [u8; 32], pk: &PublicKey) -> bool {
        let secp = Secp256k1::verification_only();
        bitcoin::secp256k1::ecdsa::Signature::from_der(&sig[..sig.len() - 1])
            .is_ok_and(|s| secp.verify_ecdsa(&bitcoin::secp256k1::Message::from_digest(msg), &s, pk).is_ok())
    }

    /// The match is cancelled before the participant funded: it signs the
    /// initiator's XBT straight back, and the initiator finishes the release
    /// with its own signature. No preimage anywhere.
    #[test]
    fn a_cancelled_swap_is_released_back_to_the_funder() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, bob, mut am, bm, preimage) = pair();
        let c = am.build_our_contract(&xbt).unwrap().clone();
        assert!(c.has_release());
        let op = OutPoint::new(Txid::from_byte_array([7; 32]), 1);
        let to = ScriptBuf::from(vec![0u8; 22]);

        let asks = am.release_txs(op, to.clone(), &[2, 20]).unwrap();
        assert_eq!(asks.len(), 2);
        for ask in &asks {
            let their = bm.sign_their_release(ask, op, &bob.xbt, &xbt).unwrap();
            let done = am.finish_release(ask, op, &to, &their, &alice.xbt).unwrap();
            let w = &done.input[0].witness;
            assert_eq!(w.len(), 5);
            let msg = c.spend_sighash(&done, 0).unwrap();
            assert!(verifies(w.nth(0).unwrap(), msg, &bob.xbt.pubkey()), "bob's (redeem) signature");
            assert!(verifies(w.nth(1).unwrap(), msg, &alice.xbt.pubkey()), "alice's (refund) signature");
            assert_eq!(done.output[0].script_pubkey, to);
            assert_eq!(preimage_from_witness(w, &sha256::Hash::hash(&preimage)), None);
        }
    }

    /// The participant signs a release of that one contract only: a single
    /// input, the contract's own outpoint.
    #[test]
    fn the_counterparty_signs_a_release_of_that_contract_only() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (_alice, bob, mut am, bm, _p) = pair();
        am.build_our_contract(&xbt).unwrap();
        let op = OutPoint::new(Txid::from_byte_array([7; 32]), 1);
        let ask = am.release_txs(op, ScriptBuf::from(vec![0u8; 22]), &[2]).unwrap().remove(0);
        let elsewhere = OutPoint::new(Txid::from_byte_array([8; 32]), 1);
        assert!(bm.sign_their_release(&ask, elsewhere, &bob.xbt, &xbt).is_err(), "another outpoint");
        let mut two = ask.clone();
        two.input.push(two.input[0].clone());
        two.input[1].previous_output = elsewhere;
        assert!(bm.sign_their_release(&two, op, &bob.xbt, &xbt).is_err(), "a second input");
        assert!(bm.sign_their_release(&ask, op, &bob.btc, &xbt).is_err(), "the wrong key");
    }

    /// The funder broadcasts only a release paying itself, with a counterparty
    /// signature that verifies.
    #[test]
    fn the_funder_finishes_only_a_good_release_to_itself() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, bob, mut am, bm, _p) = pair();
        am.build_our_contract(&xbt).unwrap();
        let op = OutPoint::new(Txid::from_byte_array([7; 32]), 1);
        let to = ScriptBuf::from(vec![0u8; 22]);
        let ask = am.release_txs(op, to.clone(), &[2]).unwrap().remove(0);
        let their = bm.sign_their_release(&ask, op, &bob.xbt, &xbt).unwrap();

        let forged = alice.btc.sign(&[3u8; 32]);
        assert!(am.finish_release(&ask, op, &to, &forged, &alice.xbt).is_err(), "a signature that doesn't verify");
        let mut elsewhere = ask.clone();
        elsewhere.output[0].script_pubkey = ScriptBuf::from(vec![1u8; 22]);
        assert!(am.finish_release(&elsewhere, op, &to, &their, &alice.xbt).is_err(), "not paying us");
        let mut pricey = ask.clone();
        pricey.output[0].value = Amount::from_sat(1);
        assert!(am.finish_release(&pricey, op, &to, &their, &alice.xbt).is_err(), "a fee over the cap");
    }

    #[test]
    fn a_v1_swap_has_no_release() {
        let (_btc, xbt) = network_params("bitcoin").unwrap();
        let (alice, bob) = (Party::new(0xa1), Party::new(0xb0));
        let t = SwapTerms { contract_version: crate::htlc::CONTRACT_V1, ..terms() };
        let h = sha256::Hash::hash(b"p");
        let mut am = SwapMachine::new(params_from_terms(&t, SwapRole::Initiator, alice.pks(), bob.pks(), h, [1; 16]));
        am.build_our_contract(&xbt).unwrap();
        assert!(am.release_txs(OutPoint::null(), ScriptBuf::new(), &[2]).is_err());
    }
}
