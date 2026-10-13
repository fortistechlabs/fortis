//! The Hash Time-Locked Contract used for one swap leg, in two versions. The
//! version is fixed by the match's terms, so a contract already funded keeps
//! the script it was funded with.
//!
//! **v1** (P2WSH), the well-tested Decred / Lightning shape:
//!
//! ```text
//! OP_IF
//!     OP_SIZE <32> OP_EQUALVERIFY OP_SHA256 <hashlock> OP_EQUALVERIFY
//!     OP_DUP OP_HASH160 <redeem_pkh>
//! OP_ELSE
//!     <locktime> OP_CHECKLOCKTIMEVERIFY OP_DROP
//!     OP_DUP OP_HASH160 <refund_pkh>
//! OP_ENDIF
//! OP_EQUALVERIFY OP_CHECKSIG
//! ```
//!
//! Redeem witness: `[sig, pubkey, preimage, 0x01, witnessScript]`.
//! Refund witness: `[sig, pubkey, <empty>, witnessScript]`.
//!
//! **v2** adds a cooperative **release**: both parties sign, no secret, no
//! timelock. When a match is cancelled before the counterparty funded, the
//! counterparty signs the funder's coins straight back to it; nothing secret is
//! revealed, so nobody can race the release with a claim.
//!
//! ```text
//! OP_IF
//!     OP_SIZE <32> OP_EQUALVERIFY OP_SHA256 <hashlock> OP_EQUALVERIFY
//!     OP_DUP OP_HASH160 <redeem_pkh> OP_EQUALVERIFY OP_CHECKSIG
//! OP_ELSE
//!     OP_IF
//!         <locktime> OP_CHECKLOCKTIMEVERIFY OP_DROP
//!         OP_DUP OP_HASH160 <refund_pkh> OP_EQUALVERIFY OP_CHECKSIG
//!     OP_ELSE
//!         <refund_pubkey> OP_CHECKSIGVERIFY <redeem_pubkey> OP_CHECKSIG
//!     OP_ENDIF
//! OP_ENDIF
//! ```
//!
//! Redeem witness: `[sig, pubkey, preimage, 0x01, witnessScript]` (as v1).
//! Refund witness: `[sig, pubkey, 0x01, <empty>, witnessScript]`.
//! Release witness: `[redeem_sig, refund_sig, <empty>, <empty>, witnessScript]`.

use bitcoin::hashes::{sha256, Hash};
use bitcoin::opcodes::all as op;
use bitcoin::script::{Builder, Instruction};
use bitcoin::secp256k1::PublicKey;
use bitcoin::{
    absolute, transaction, Amount, OutPoint, Script, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Witness,
};
use serde::{Deserialize, Serialize};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::sighash::{sighash_all, SighashVariant};
use crate::types::Preimage;

/// The 0-value OP_RETURN payload every BTC escrow spend carries: 100 bytes,
/// over the BLAKE2b fork's 82-byte consensus datacarrier cap, so the spend can
/// never be mined on XBT (a BTC signature is otherwise valid there). Fixed,
/// not random, so a rebuilt spend matches the one signed before. XBT spends
/// need none: their `SIGHASH_UNIFIED` signatures are invalid on BTC.
pub const REPLAY_GUARD: [u8; 100] = *b"fortis swap replay guard: over the BLAKE2b fork's 82-byte OP_RETURN cap, so never valid on XBT......";

/// The original contract: redeem or refund.
pub const CONTRACT_V1: u8 = 1;
/// Adds the cooperative release.
pub const CONTRACT_V2: u8 = 2;
/// What new matches use.
pub const CURRENT_CONTRACT: u8 = CONTRACT_V2;

fn v1() -> u8 {
    CONTRACT_V1
}

/// Which way a contract is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtlcPath {
    Redeem,
    Refund,
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtlcContract {
    /// [`CONTRACT_V1`] or [`CONTRACT_V2`]; a session saved before versions
    /// existed holds a v1 contract.
    #[serde(default = "v1")]
    pub version: u8,
    pub witness_script: ScriptBuf,
    pub script_pubkey: ScriptBuf,
    pub value: Amount,
    pub hashlock: sha256::Hash,
    /// Absolute CLTV lock time (unix seconds) guarding the refund path.
    pub locktime: u32,
    pub sighash_variant: SighashVariant,
}

impl HtlcContract {
    /// `redeem_pubkey` spends via the hashlock; `refund_pubkey` via the
    /// timelock (and, v2, both together via the release). `version` comes from
    /// the match's terms, which only ever carry a known one.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        params: &ChainParams,
        version: u8,
        hashlock: sha256::Hash,
        redeem_pubkey: &PublicKey,
        refund_pubkey: &PublicKey,
        locktime: u32,
        value: Amount,
    ) -> Self {
        Self::try_build(params, version, hashlock, redeem_pubkey, refund_pubkey, locktime, value)
            .expect("terms carry a known contract version")
    }

    /// [`Self::build`], refusing an unknown `version`.
    #[allow(clippy::too_many_arguments)]
    pub fn try_build(
        params: &ChainParams,
        version: u8,
        hashlock: sha256::Hash,
        redeem_pubkey: &PublicKey,
        refund_pubkey: &PublicKey,
        locktime: u32,
        value: Amount,
    ) -> Result<Self> {
        let witness_script = match version {
            CONTRACT_V1 => contract_script(hashlock, redeem_pubkey, refund_pubkey, locktime),
            CONTRACT_V2 => contract_script_v2(hashlock, redeem_pubkey, refund_pubkey, locktime),
            v => return Err(WalletError::InvalidSwapParams(format!("unknown contract version {v}"))),
        };
        let script_pubkey = ScriptBuf::new_p2wsh(&witness_script.wscript_hash());
        Ok(Self {
            version,
            witness_script,
            script_pubkey,
            value,
            hashlock,
            locktime,
            sighash_variant: params.sighash_variant(),
        })
    }

    /// The contract has the cooperative release branch (v2).
    pub fn has_release(&self) -> bool {
        self.version >= CONTRACT_V2
    }

    pub fn funding_output(&self) -> TxOut {
        TxOut { value: self.value, script_pubkey: self.script_pubkey.clone() }
    }

    /// Find the output in `tx` that funds this exact contract with at least `min_value`.
    /// Used to verify a counterparty's on-chain funding transaction.
    pub fn find_output(&self, tx: &Transaction, min_value: Amount) -> Option<(u32, Amount)> {
        tx.output.iter().enumerate().find_map(|(i, o)| {
            (o.script_pubkey == self.script_pubkey && o.value >= min_value)
                .then_some((i as u32, o.value))
        })
    }

    /// Find the output in `tx` that funds this exact contract with exactly `value`.
    /// Swaps require an exact amount: redeem signing commits to the stored contract
    /// value, so an over-funded output would make our redeem consensus-invalid.
    pub fn find_exact_output(&self, tx: &Transaction, value: Amount) -> Option<u32> {
        tx.output
            .iter()
            .position(|o| o.script_pubkey == self.script_pubkey && o.value == value)
            .map(|i| i as u32)
    }

    fn spend_skeleton(
        &self,
        prevout: OutPoint,
        to: ScriptBuf,
        fee: Amount,
        lock_time: absolute::LockTime,
        sequence: Sequence,
    ) -> Result<Transaction> {
        let value = self.value.checked_sub(fee).ok_or(WalletError::InsufficientFunds {
            need: fee.to_sat(),
            have: self.value.to_sat(),
        })?;
        Ok(Transaction {
            version: transaction::Version::TWO,
            lock_time,
            input: vec![TxIn {
                previous_output: prevout,
                script_sig: ScriptBuf::new(),
                sequence,
                witness: Witness::new(),
            }],
            output: self.spend_outputs(value, to),
        })
    }

    /// The payout, then — on a chain whose signatures don't bind it
    /// (`SegwitV0`: BTC) — the [`REPLAY_GUARD`] OP_RETURN.
    fn spend_outputs(&self, value: Amount, to: ScriptBuf) -> Vec<TxOut> {
        let mut out = vec![TxOut { value, script_pubkey: to }];
        if self.sighash_variant == SighashVariant::SegwitV0 {
            // The send path's own OP_RETURN builder; 100 bytes always fit a push.
            out.push(crate::op_return_output(&REPLAY_GUARD).expect("a 100-byte push"));
        }
        out
    }

    /// Unsigned redeem transaction (hashlock path).
    pub fn redeem_tx(&self, prevout: OutPoint, to: ScriptBuf, fee: Amount) -> Result<Transaction> {
        self.spend_skeleton(prevout, to, fee, absolute::LockTime::ZERO, Sequence::ENABLE_RBF_NO_LOCKTIME)
    }

    /// Unsigned refund transaction (timelock path); `nLockTime` = the contract CLTV.
    pub fn refund_tx(&self, prevout: OutPoint, to: ScriptBuf, fee: Amount) -> Result<Transaction> {
        self.spend_skeleton(
            prevout,
            to,
            fee,
            absolute::LockTime::from_consensus(self.locktime),
            Sequence::ENABLE_RBF_NO_LOCKTIME,
        )
    }

    /// Unsigned release transaction (v2, both sign): no timelock, so the
    /// counterparty can hand the funder its coins back at once.
    pub fn release_tx(&self, prevout: OutPoint, to: ScriptBuf, fee: Amount) -> Result<Transaction> {
        if !self.has_release() {
            return Err(WalletError::InvalidSwapParams("this contract has no release branch".into()));
        }
        self.spend_skeleton(prevout, to, fee, absolute::LockTime::ZERO, Sequence::ENABLE_RBF_NO_LOCKTIME)
    }

    /// Attach the release witness in place: the redeem party's and the refund
    /// party's signatures (each DER + hashtype byte), both over
    /// [`Self::spend_sighash`].
    pub fn finalize_release(&self, tx: &mut Transaction, input_index: usize, redeem_sig: &[u8], refund_sig: &[u8]) -> Result<()> {
        if !self.has_release() {
            return Err(WalletError::InvalidSwapParams("this contract has no release branch".into()));
        }
        tx.input[input_index].witness = release_witness(redeem_sig, refund_sig, &self.witness_script);
        Ok(())
    }

    /// Sighash message for spending this contract (input `input_index` of a
    /// single-input redeem/refund tx). Sign it with [`crate::MasterKey::sign_swap`].
    pub fn spend_sighash(&self, tx: &Transaction, input_index: usize) -> Result<[u8; 32]> {
        let prevouts = [TxOut { value: self.value, script_pubkey: self.script_pubkey.clone() }];
        Ok(sighash_all(tx, &prevouts, input_index, &self.witness_script, self.sighash_variant)?.message)
    }

    /// Attach the redeem witness in place. `sig` is the DER signature + hashtype byte
    /// (the output of `MasterKey::sign_swap`).
    pub fn finalize_redeem(
        &self,
        tx: &mut Transaction,
        input_index: usize,
        sig: &[u8],
        redeem_pubkey: &PublicKey,
        preimage: &Preimage,
    ) {
        tx.input[input_index].witness =
            redeem_witness(sig, redeem_pubkey, preimage, &self.witness_script);
    }

    /// Attach the refund witness in place.
    pub fn finalize_refund(
        &self,
        tx: &mut Transaction,
        input_index: usize,
        sig: &[u8],
        refund_pubkey: &PublicKey,
    ) {
        tx.input[input_index].witness = refund_witness(self.version, sig, &refund_pubkey.serialize(), &self.witness_script);
    }
}

/// The timelock-path witness: v1 `[sig, pubkey, <empty>, ws]`, v2 picks the
/// inner branch too: `[sig, pubkey, 0x01, <empty>, ws]`.
fn refund_witness(version: u8, sig: &[u8], pubkey: &[u8], witness_script: &Script) -> Witness {
    let mut w = Witness::new();
    w.push(sig);
    w.push(pubkey);
    if version >= CONTRACT_V2 {
        w.push([1u8]);
    }
    w.push::<&[u8]>(&[]);
    w.push(witness_script.as_bytes());
    w
}

/// The release witness: `[redeem_sig, refund_sig, <empty>, <empty>, ws]` —
/// `refund_sig` on top for the first `OP_CHECKSIGVERIFY`.
fn release_witness(redeem_sig: &[u8], refund_sig: &[u8], witness_script: &Script) -> Witness {
    let mut w = Witness::new();
    w.push(redeem_sig);
    w.push(refund_sig);
    w.push::<&[u8]>(&[]);
    w.push::<&[u8]>(&[]);
    w.push(witness_script.as_bytes());
    w
}

/// The hashlock-path witness: `[sig, pubkey, preimage, 0x01, witnessScript]`.
pub(crate) fn redeem_witness(
    sig: &[u8],
    redeem_pubkey: &PublicKey,
    preimage: &Preimage,
    witness_script: &Script,
) -> Witness {
    let mut w = Witness::new();
    w.push(sig);
    w.push(redeem_pubkey.serialize());
    w.push(preimage.as_slice());
    w.push([1u8]);
    w.push(witness_script.as_bytes());
    w
}

/// The hashlock committed in an HTLC witness script (the 32-byte push after `OP_SHA256`).
pub(crate) fn script_hashlock(witness_script: &Script) -> Option<sha256::Hash> {
    let mut it = witness_script.instructions();
    while let Some(Ok(ins)) = it.next() {
        if ins == Instruction::Op(op::OP_SHA256) {
            return match it.next() {
                Some(Ok(Instruction::PushBytes(b))) => sha256::Hash::from_slice(b.as_bytes()).ok(),
                _ => None,
            };
        }
    }
    None
}

/// Fee for a single-input spend of `contract` along `path` at
/// `feerate_sat_vb`. Sized from the finished witness with 72-byte signatures
/// and a 34-byte output script, so it never undershoots for P2WPKH, P2WSH or
/// P2TR payouts. A release of a v1 contract (which has none) is priced as a
/// refund.
pub fn htlc_spend_fee(contract: &HtlcContract, path: HtlcPath, feerate_sat_vb: u64) -> Amount {
    let to = ScriptBuf::from(vec![0u8; 34]);
    let (sig, pk) = ([0u8; 72], [0u8; 33]);
    let path = if path == HtlcPath::Release && !contract.has_release() { HtlcPath::Refund } else { path };
    let mut tx = match path {
        HtlcPath::Redeem => contract.redeem_tx(OutPoint::null(), to, Amount::ZERO),
        HtlcPath::Refund => contract.refund_tx(OutPoint::null(), to, Amount::ZERO),
        HtlcPath::Release => contract.release_tx(OutPoint::null(), to, Amount::ZERO),
    }
    .expect("a zero fee never exceeds the contract value");
    tx.input[0].witness = match path {
        HtlcPath::Redeem => Witness::from_slice(&[&sig[..], &pk, &[0u8; 32], &[1u8], contract.witness_script.as_bytes()]),
        HtlcPath::Refund => refund_witness(contract.version, &sig, &pk, &contract.witness_script),
        HtlcPath::Release => release_witness(&sig, &sig, &contract.witness_script),
    };
    Amount::from_sat((tx.vsize() as u64).saturating_mul(feerate_sat_vb))
}


/// The swap preimage revealed by a spend of an HTLC input: any 32-byte witness
/// item whose SHA-256 is `hashlock`.
///
/// Found by what it is, never by where it sits: our own redeem witness is
/// `[sig, pubkey, preimage, 0x01, witnessScript]`, but the script's `OP_IF`
/// takes any true value (MINIMALIF is only relay policy for witness v0), so a
/// counterparty can mine a redeem with `0x02` there, or another layout. Missing
/// that would read a redeem as "no preimage" — and the victim as refunded.
/// A refund carries no preimage: `None`.
pub fn preimage_from_witness(witness: &Witness, hashlock: &sha256::Hash) -> Option<Preimage> {
    witness
        .iter()
        .filter_map(|item| <Preimage>::try_from(item).ok())
        .find(|p| sha256::Hash::hash(p) == *hashlock)
}

fn contract_script(
    hashlock: sha256::Hash,
    redeem_pubkey: &PublicKey,
    refund_pubkey: &PublicKey,
    locktime: u32,
) -> ScriptBuf {
    let redeem_pkh = bitcoin::PublicKey::new(*redeem_pubkey).pubkey_hash();
    let refund_pkh = bitcoin::PublicKey::new(*refund_pubkey).pubkey_hash();
    Builder::new()
        .push_opcode(op::OP_IF)
        .push_opcode(op::OP_SIZE)
        .push_int(32)
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_SHA256)
        .push_slice(hashlock.to_byte_array())
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_DUP)
        .push_opcode(op::OP_HASH160)
        .push_slice(redeem_pkh.to_byte_array())
        .push_opcode(op::OP_ELSE)
        .push_int(i64::from(locktime))
        .push_opcode(op::OP_CLTV)
        .push_opcode(op::OP_DROP)
        .push_opcode(op::OP_DUP)
        .push_opcode(op::OP_HASH160)
        .push_slice(refund_pkh.to_byte_array())
        .push_opcode(op::OP_ENDIF)
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_CHECKSIG)
        .into_script()
}

/// The v2 script (see the module docs).
fn contract_script_v2(
    hashlock: sha256::Hash,
    redeem_pubkey: &PublicKey,
    refund_pubkey: &PublicKey,
    locktime: u32,
) -> ScriptBuf {
    let redeem_pkh = bitcoin::PublicKey::new(*redeem_pubkey).pubkey_hash();
    let refund_pkh = bitcoin::PublicKey::new(*refund_pubkey).pubkey_hash();
    Builder::new()
        .push_opcode(op::OP_IF)
        .push_opcode(op::OP_SIZE)
        .push_int(32)
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_SHA256)
        .push_slice(hashlock.to_byte_array())
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_DUP)
        .push_opcode(op::OP_HASH160)
        .push_slice(redeem_pkh.to_byte_array())
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_CHECKSIG)
        .push_opcode(op::OP_ELSE)
        .push_opcode(op::OP_IF)
        .push_int(i64::from(locktime))
        .push_opcode(op::OP_CLTV)
        .push_opcode(op::OP_DROP)
        .push_opcode(op::OP_DUP)
        .push_opcode(op::OP_HASH160)
        .push_slice(refund_pkh.to_byte_array())
        .push_opcode(op::OP_EQUALVERIFY)
        .push_opcode(op::OP_CHECKSIG)
        .push_opcode(op::OP_ELSE)
        .push_slice(refund_pubkey.serialize())
        .push_opcode(op::OP_CHECKSIGVERIFY)
        .push_slice(redeem_pubkey.serialize())
        .push_opcode(op::OP_CHECKSIG)
        .push_opcode(op::OP_ENDIF)
        .push_opcode(op::OP_ENDIF)
        .into_script()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};

    fn pk(byte: u8) -> PublicKey {
        let secp = Secp256k1::new();
        PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[byte; 32]).unwrap())
    }

    fn contract() -> HtlcContract {
        HtlcContract::build(
            &ChainParams::bitcoin(),
            CURRENT_CONTRACT,
            sha256::Hash::hash(b"secret"),
            &pk(1),
            &pk(2),
            1_800_000_000,
            Amount::from_sat(100_000),
        )
    }

    fn contract_on(params: &ChainParams) -> HtlcContract {
        HtlcContract::build(params, CURRENT_CONTRACT, sha256::Hash::hash(b"secret"), &pk(1), &pk(2), 1_800_000_000, Amount::from_sat(100_000))
    }

    fn payout() -> ScriptBuf {
        ScriptBuf::new_p2wsh(&ScriptBuf::new().wscript_hash())
    }

    /// The bytes of `o`'s single OP_RETURN push, if it is an OP_RETURN.
    fn op_return_data(o: &TxOut) -> Option<Vec<u8>> {
        let mut it = o.script_pubkey.instructions();
        (it.next()? == Ok(Instruction::Op(op::OP_RETURN))).then_some(())?;
        match it.next()? {
            Ok(Instruction::PushBytes(b)) => Some(b.as_bytes().to_vec()),
            _ => None,
        }
    }

    /// A BTC spend's signature is also valid on the fork, so every BTC redeem
    /// and refund carries an OP_RETURN over the fork's 82-byte consensus cap:
    /// it can never be mined there. The payout stays output 0.
    #[test]
    fn btc_spends_carry_a_replay_guard() {
        for params in [ChainParams::bitcoin(), ChainParams::bitcoin_regtest()] {
            let c = contract_on(&params);
            let fee = Amount::from_sat(1_000);
            for tx in [c.redeem_tx(OutPoint::null(), payout(), fee).unwrap(), c.refund_tx(OutPoint::null(), payout(), fee).unwrap()] {
                assert_eq!(tx.output.len(), 2);
                assert_eq!(tx.output[0], TxOut { value: Amount::from_sat(99_000), script_pubkey: payout() });
                assert_eq!(tx.output[1].value, Amount::ZERO);
                let data = op_return_data(&tx.output[1]).expect("an OP_RETURN");
                assert!(data.len() > 82, "over the fork's cap: {}", data.len());
                assert_eq!(data.len(), REPLAY_GUARD.len());
            }
        }
    }

    /// XBT spends need no guard output: `SIGHASH_UNIFIED` signatures are
    /// invalid on BTC.
    #[test]
    fn xbt_spends_rely_on_the_unified_sighash() {
        for params in [ChainParams::blake2b(), ChainParams::blake2b_regtest()] {
            let c = contract_on(&params);
            assert_eq!(c.sighash_variant, SighashVariant::Unified);
            let tx = c.redeem_tx(OutPoint::null(), payout(), Amount::from_sat(1_000)).unwrap();
            assert_eq!(tx.output.len(), 1);
        }
    }

    /// The fee estimate sizes the real transaction, guard included: 112 vB
    /// more on BTC (8 value + 1 length + OP_RETURN, OP_PUSHDATA1, 100, data).
    #[test]
    fn the_btc_fee_estimate_pays_for_the_guard() {
        let (btc, xbt) = (contract_on(&ChainParams::bitcoin()), contract_on(&ChainParams::blake2b()));
        for path in [HtlcPath::Redeem, HtlcPath::Refund, HtlcPath::Release] {
            let extra = htlc_spend_fee(&btc, path, 3) - htlc_spend_fee(&xbt, path, 3);
            assert_eq!(extra, Amount::from_sat(112 * 3));
        }
    }

    #[test]
    fn script_pubkey_is_p2wsh() {
        let c = contract();
        assert!(c.script_pubkey.is_p2wsh());
        assert_eq!(c.script_pubkey.len(), 34);
    }

    #[test]
    fn witness_script_has_hashlock_and_cltv() {
        let c = contract();
        let asm = c.witness_script.to_asm_string();
        assert!(asm.contains("OP_SHA256"));
        assert!(asm.contains("OP_CLTV") || asm.contains("OP_CHECKLOCKTIMEVERIFY") || asm.contains("OP_NOP2"));
    }

    #[test]
    fn redeem_and_refund_tx_shape() {
        let c = contract();
        let op = OutPoint::null();
        let spk = ScriptBuf::new();
        let redeem = c.redeem_tx(op, spk.clone(), Amount::from_sat(500)).unwrap();
        assert_eq!(redeem.lock_time, absolute::LockTime::ZERO);
        assert_eq!(redeem.input[0].sequence, Sequence::ENABLE_RBF_NO_LOCKTIME);
        assert_eq!(redeem.output[0].value, Amount::from_sat(99_500));

        let refund = c.refund_tx(op, spk, Amount::from_sat(500)).unwrap();
        assert_eq!(refund.lock_time, absolute::LockTime::from_consensus(1_800_000_000));
        assert_eq!(refund.input[0].sequence, Sequence::ENABLE_RBF_NO_LOCKTIME);
    }

    #[test]
    fn fee_larger_than_value_is_rejected() {
        let c = contract();
        let err = c.redeem_tx(OutPoint::null(), ScriptBuf::new(), Amount::from_sat(200_000));
        assert!(err.is_err());
    }

    #[test]
    fn xbt_redeem_roundtrip_uses_unified_sighash_and_verifies() {
        use crate::keys::MasterKey;
        use bitcoin::secp256k1::Message;

        let (_m, key) = MasterKey::generate(&[42u8; 32]).unwrap();
        let xbt = ChainParams::blake2b();
        let redeem_pk = key.swap_pubkey(&xbt, 0, 3).unwrap();
        let refund_pk = pk(9);
        let preimage = [7u8; 32];
        let hashlock = sha256::Hash::hash(&preimage);

        let c = HtlcContract::build(
            &xbt,
            CURRENT_CONTRACT,
            hashlock,
            &redeem_pk,
            &refund_pk,
            1_800_000_000,
            Amount::from_sat(500_000),
        );
        assert_eq!(c.sighash_variant, SighashVariant::Unified);

        let prevout = OutPoint::new(bitcoin::Txid::from_byte_array([1u8; 32]), 0);
        let mut tx = c.redeem_tx(prevout, ScriptBuf::from(vec![0u8; 22]), Amount::from_sat(400)).unwrap();

        let msg = c.spend_sighash(&tx, 0).unwrap();
        let sig = key.sign_swap(&xbt, 0, 3, &msg).unwrap();
        assert_eq!(sig.last(), Some(&0x21)); // ALL | UNIFIED

        c.finalize_redeem(&mut tx, 0, &sig, &redeem_pk, &preimage);
        let w = &tx.input[0].witness;
        assert_eq!(w.len(), 5);
        assert_eq!(w.nth(2).unwrap(), &preimage[..]);
        assert_eq!(w.nth(3).unwrap(), &[1u8]);
        assert_eq!(w.nth(4).unwrap(), c.witness_script.as_bytes());
        assert_eq!(preimage_from_witness(w, &hashlock), Some(preimage));

        let mut refund = c.refund_tx(prevout, ScriptBuf::from(vec![0u8; 22]), Amount::from_sat(400)).unwrap();
        c.finalize_refund(&mut refund, 0, b"fake-sig", &refund_pk);
        assert_eq!(preimage_from_witness(&refund.input[0].witness, &hashlock), None);

        // The signature (minus the sighash-flag byte) verifies over the unified message.
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();
        let parsed = bitcoin::secp256k1::ecdsa::Signature::from_der(&sig[..sig.len() - 1]).unwrap();
        secp.verify_ecdsa(&Message::from_digest(msg), &parsed, &redeem_pk).unwrap();
    }

    /// The redeem branch's `OP_IF` takes any true value, and MINIMALIF is only
    /// relay policy for witness v0: a counterparty can mine a redeem whose if-
    /// argument is `0x02` (or padded). The preimage is found by what it is — a
    /// 32-byte item hashing to the hashlock — never by its position or flag.
    #[test]
    fn the_preimage_is_found_whatever_the_if_argument_or_layout() {
        let preimage = [7u8; 32];
        let hashlock = sha256::Hash::hash(&preimage);
        let (sig, pk_bytes, ws) = ([1u8; 71], [2u8; 33], [0x63u8; 40]);
        for flag in [&[0x01u8][..], &[0x02], &[0xff], &[0x01, 0x00]] {
            let w = Witness::from_slice(&[&sig[..], &pk_bytes, &preimage, flag, &ws]);
            assert_eq!(preimage_from_witness(&w, &hashlock), Some(preimage), "flag {flag:?}");
        }
        // Not tied to a 5-item layout either.
        let w = Witness::from_slice(&[&sig[..], &preimage, &pk_bytes, &[0x01], &ws, &[]]);
        assert_eq!(preimage_from_witness(&w, &hashlock), Some(preimage));
        // A 32-byte item that doesn't open the hashlock is not a preimage.
        let w = Witness::from_slice(&[&sig[..], &pk_bytes, &[9u8; 32], &[0x01], &ws]);
        assert_eq!(preimage_from_witness(&w, &hashlock), None);
        // A refund has none.
        let w = Witness::from_slice(&[&sig[..], &pk_bytes, &[], &ws]);
        assert_eq!(preimage_from_witness(&w, &hashlock), None);
    }

    // ---- contract versions: v1 (redeem | refund), v2 (+ cooperative release) ----

    /// `v(CONTRACT_V1)`'s script, as built before versions existed.
    const V1_SCRIPT_HEX: &str = "6382012088a8202bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b8876a91479b000887626b294a914501a4cd226b58b235983670400d2496bb17576a914ebc0ee0b2ab9e8277a600c251475e22a3241a1c16888ac";

    fn v(version: u8) -> HtlcContract {
        HtlcContract::build(&ChainParams::bitcoin(), version, sha256::Hash::hash(b"secret"), &pk(1), &pk(2), 1_800_000_000, Amount::from_sat(100_000))
    }

    /// A v1 contract is byte-for-byte the script swaps already funded use, and
    /// refunds the way it always did: in-flight swaps must stay spendable.
    #[test]
    fn a_v1_contract_keeps_its_script_and_refund_witness() {
        let c = v(CONTRACT_V1);
        assert!(!c.witness_script.to_asm_string().contains("OP_CHECKSIGVERIFY"));
        assert_eq!(hex::encode(c.witness_script.as_bytes()), V1_SCRIPT_HEX, "the v1 script, unchanged");
        let mut refund = c.refund_tx(OutPoint::null(), payout(), Amount::from_sat(500)).unwrap();
        c.finalize_refund(&mut refund, 0, b"sig", &pk(2));
        let w = &refund.input[0].witness;
        assert_eq!(w.len(), 4);
        assert!(w.nth(2).unwrap().is_empty());
        assert!(c.release_tx(OutPoint::null(), payout(), Amount::from_sat(500)).is_err(), "v1 has no release branch");
    }

    /// v2 adds a branch both parties sign, no secret, no timelock: the
    /// counterparty can hand the funder its coins back at once.
    #[test]
    fn a_v2_contract_has_a_release_branch_both_must_sign() {
        let c = v(CONTRACT_V2);
        let asm = c.witness_script.to_asm_string();
        assert!(asm.contains("OP_CHECKSIGVERIFY"));
        assert_eq!(script_hashlock(&c.witness_script), Some(sha256::Hash::hash(b"secret")));
        assert_ne!(c.script_pubkey, v(CONTRACT_V1).script_pubkey);

        let release = c.release_tx(OutPoint::null(), payout(), Amount::from_sat(500)).unwrap();
        assert_eq!(release.lock_time, absolute::LockTime::ZERO);
        assert_eq!(release.input[0].sequence, Sequence::ENABLE_RBF_NO_LOCKTIME);
        assert_eq!(release.output[0].value, Amount::from_sat(99_500));
        let mut done = release.clone();
        c.finalize_release(&mut done, 0, b"redeem-sig", b"refund-sig").unwrap();
        let w = &done.input[0].witness;
        assert_eq!(w.len(), 5);
        assert_eq!(w.nth(0).unwrap(), b"redeem-sig");
        assert_eq!(w.nth(1).unwrap(), b"refund-sig");
        assert!(w.nth(2).unwrap().is_empty() && w.nth(3).unwrap().is_empty());
        assert_eq!(w.nth(4).unwrap(), c.witness_script.as_bytes());
        assert_eq!(preimage_from_witness(w, &sha256::Hash::hash(b"secret")), None, "a release reveals no secret");

        // the refund now picks its branch with a 0x01
        let mut refund = c.refund_tx(OutPoint::null(), payout(), Amount::from_sat(500)).unwrap();
        c.finalize_refund(&mut refund, 0, b"sig", &pk(2));
        let w = &refund.input[0].witness;
        assert_eq!(w.len(), 5);
        assert_eq!(w.nth(2).unwrap(), &[1u8]);
        assert!(w.nth(3).unwrap().is_empty());
    }

    /// Fee estimates are sized from each path's finished witness.
    #[test]
    fn each_path_is_priced_from_its_own_witness() {
        let c = v(CONTRACT_V2);
        let (redeem, refund, release) = (
            htlc_spend_fee(&c, HtlcPath::Redeem, 10),
            htlc_spend_fee(&c, HtlcPath::Refund, 10),
            htlc_spend_fee(&c, HtlcPath::Release, 10),
        );
        assert!(release > refund, "two signatures: {release} vs {refund}");
        assert!(redeem > refund);
        assert!(htlc_spend_fee(&v(CONTRACT_V1), HtlcPath::Refund, 10) < refund, "v1's refund witness is smaller");
    }

    #[test]
    fn an_unknown_contract_version_is_refused() {
        assert!(HtlcContract::try_build(&ChainParams::bitcoin(), 3, sha256::Hash::hash(b"s"), &pk(1), &pk(2), 1, Amount::from_sat(1)).is_err());
    }
}
