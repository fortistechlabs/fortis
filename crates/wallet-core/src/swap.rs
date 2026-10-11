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
use crate::htlc::{htlc_spend_fee, redeem_witness, script_hashlock, HtlcContract};
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
        let c = HtlcContract::build(
            send_params,
            self.params.hashlock,
            &self.params.their_pubkey_send,
            &self.params.our_pubkey_send,
            self.params.our_contract_locktime,
            self.params.send_amount,
        );
        Ok(self.our_contract.insert(c))
    }

    /// The contract we expect the counterparty to have funded on the receive chain.
    pub fn expected_their_contract(&self, recv_params: &ChainParams) -> HtlcContract {
        HtlcContract::build(
            recv_params,
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
        check_feerates(feerates)?;
        feerates
            .iter()
            .map(|&rate| {
                let mut tx = c.refund_tx(outpoint, to.clone(), htlc_spend_fee(c, false, rate))?;
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
        check_feerates(feerates)?;
        feerates
            .iter()
            .map(|&rate| {
                let tx = c.redeem_tx(outpoint, to.clone(), htlc_spend_fee(c, true, rate))?;
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

fn check_feerates(feerates: &[u64]) -> Result<()> {
    if feerates.is_empty() {
        return Err(WalletError::InvalidInput("no feerates given".into()));
    }
    Ok(())
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
            xbt_locktime: MATCHED_AT + 144 * 3600,
            btc_locktime: MATCHED_AT + 24 * 3600,
            xbt_confs: 6,
            btc_confs: 2,
            accept_by: MATCHED_AT + 120,
            initiator_fund_by: MATCHED_AT + 1920,
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
        let preimage = alice.xbt.preimage();
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
        assert_eq!(a_xbt.locktime, MATCHED_AT + 144 * 3600);
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
        assert_eq!(fee_of(&c, &txs[0]), htlc_spend_fee(&c, false, 2).to_sat());
        for tx in &txs {
            assert_eq!(tx.input[0].witness.len(), 4);
            assert_eq!(tx.lock_time, bitcoin::absolute::LockTime::from_consensus(c.locktime));
            assert_eq!(tx.input[0].sequence, bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME);
            assert_eq!(tx.input[0].witness.nth(0).unwrap().last(), Some(&0x21));
        }
        // the wrong key is refused rather than producing an unspendable refund
        assert!(am.signed_refund(op, to, &alice.btc, &[2]).is_err());
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
        assert_eq!(preimage_from_witness(&done.input[0].witness), Some(preimage));

        // a preimage that does not open the hashlock is refused
        assert!(finish_presigned_redeem(&pre0, &[0u8; 32]).is_err());
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
}
