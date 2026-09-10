//! Client side of the HTLC atomic-swap protocol.
//!
//! The XBT holder always [`SwapRole::Initiator`]s, so the preimage is revealed by a
//! redeem on Bitcoin — the chain where a reversal is hardest.

use bitcoin::hashes::sha256;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Amount, OutPoint, ScriptBuf, Transaction, Txid};

use crate::chain::{Chain, ChainParams};
use crate::error::{Result, WalletError};
use crate::htlc::HtlcContract;
use crate::types::Preimage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapRole {
    /// Funds first; knows the preimage; reveals it by redeeming on the receive chain.
    Initiator,
    /// Funds second; redeems with the revealed preimage.
    Participant,
}

/// Terms both wallets agree (via the platform's matching + negotiation) before
/// anyone funds. A swap needs a keypair per party per chain, hence four pubkeys.
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

pub struct SwapMachine {
    params: SwapParams,
    state: SwapState,
    our_contract: Option<HtlcContract>,
    their_contract: Option<HtlcContract>,
}

impl SwapMachine {
    pub fn new(params: SwapParams) -> Self {
        Self { params, state: SwapState::Negotiated, our_contract: None, their_contract: None }
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
    fn expected_their_contract(&self, recv_params: &ChainParams) -> HtlcContract {
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
    /// exactly matches the agreed HTLC and is not short-funded. Returns the outpoint.
    pub fn verify_their_contract(
        &mut self,
        funding_tx: &[u8],
        recv_params: &ChainParams,
    ) -> Result<OutPoint> {
        let expected = self.expected_their_contract(recv_params);
        let tx: Transaction = bitcoin::consensus::deserialize(funding_tx)
            .map_err(|e| WalletError::Bitcoin(format!("funding tx decode: {e}")))?;

        let (vout, _value) = expected
            .find_output(&tx, self.params.recv_amount)
            .ok_or(WalletError::ContractMismatch)?;

        self.their_contract = Some(expected);
        Ok(OutPoint { txid: tx.compute_txid(), vout })
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

/// `(our_leg_secs, their_leg_secs)` — the initiator's leg is always the longer one.
pub fn timelocks(role: SwapRole, send: &ChainParams, _recv: &ChainParams) -> (u32, u32) {
    match role {
        SwapRole::Initiator => (send.htlc_initiator_locktime_secs, send.htlc_participant_locktime_secs),
        SwapRole::Participant => (send.htlc_participant_locktime_secs, send.htlc_initiator_locktime_secs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::consensus::encode::serialize;
    use bitcoin::hashes::Hash;
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
    fn initiator_leg_is_longer() {
        let (ours, theirs) = timelocks(SwapRole::Initiator, &ChainParams::bitcoin(), &ChainParams::blake2b());
        assert!(ours > theirs);
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
