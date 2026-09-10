//! The Hash Time-Locked Contract used for one swap leg.
//!
//! Script (P2WSH), the well-tested Decred / Lightning shape:
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

use bitcoin::hashes::{sha256, Hash};
use bitcoin::opcodes::all as op;
use bitcoin::script::Builder;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{
    absolute, transaction, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::sighash::{sighash_all, SighashVariant};
use crate::types::Preimage;

#[derive(Debug, Clone)]
pub struct HtlcContract {
    pub witness_script: ScriptBuf,
    pub script_pubkey: ScriptBuf,
    pub value: Amount,
    pub hashlock: sha256::Hash,
    /// Absolute CLTV lock time (unix seconds) guarding the refund path.
    pub locktime: u32,
    pub sighash_variant: SighashVariant,
}

impl HtlcContract {
    /// `redeem_pubkey` spends via the hashlock; `refund_pubkey` via the timelock.
    pub fn build(
        params: &ChainParams,
        hashlock: sha256::Hash,
        redeem_pubkey: &PublicKey,
        refund_pubkey: &PublicKey,
        locktime: u32,
        value: Amount,
    ) -> Self {
        let witness_script = contract_script(hashlock, redeem_pubkey, refund_pubkey, locktime);
        let script_pubkey = ScriptBuf::new_p2wsh(&witness_script.wscript_hash());
        Self {
            witness_script,
            script_pubkey,
            value,
            hashlock,
            locktime,
            sighash_variant: params.sighash_variant(),
        }
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
            output: vec![TxOut { value, script_pubkey: to }],
        })
    }

    /// Unsigned redeem transaction (hashlock path).
    pub fn redeem_tx(&self, prevout: OutPoint, to: ScriptBuf, fee: Amount) -> Result<Transaction> {
        self.spend_skeleton(prevout, to, fee, absolute::LockTime::ZERO, Sequence::MAX)
    }

    /// Unsigned refund transaction (timelock path); `nLockTime` = the contract CLTV.
    pub fn refund_tx(&self, prevout: OutPoint, to: ScriptBuf, fee: Amount) -> Result<Transaction> {
        self.spend_skeleton(
            prevout,
            to,
            fee,
            absolute::LockTime::from_consensus(self.locktime),
            Sequence::ENABLE_LOCKTIME_NO_RBF,
        )
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
        let mut w = Witness::new();
        w.push(sig);
        w.push(redeem_pubkey.serialize());
        w.push(preimage.as_slice());
        w.push([1u8]);
        w.push(self.witness_script.as_bytes());
        tx.input[input_index].witness = w;
    }

    /// Attach the refund witness in place.
    pub fn finalize_refund(
        &self,
        tx: &mut Transaction,
        input_index: usize,
        sig: &[u8],
        refund_pubkey: &PublicKey,
    ) {
        let mut w = Witness::new();
        w.push(sig);
        w.push(refund_pubkey.serialize());
        w.push::<&[u8]>(&[]);
        w.push(self.witness_script.as_bytes());
        tx.input[input_index].witness = w;
    }
}

/// Extract the swap preimage from a redeem-path spend of an HTLC output.
///
/// Our redeem witness is `[sig, pubkey, preimage, 0x01, witnessScript]`; the refund
/// witness has an empty third item. Used by the watchtower to learn the secret once
/// a counterparty redeems on-chain.
pub fn preimage_from_witness(witness: &Witness) -> Option<Preimage> {
    if witness.len() != 5 || witness.nth(3)? != [0x01] {
        return None;
    }
    witness.nth(2)?.try_into().ok()
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
            sha256::Hash::hash(b"secret"),
            &pk(1),
            &pk(2),
            1_800_000_000,
            Amount::from_sat(100_000),
        )
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
        assert_eq!(redeem.input[0].sequence, Sequence::MAX);
        assert_eq!(redeem.output[0].value, Amount::from_sat(99_500));

        let refund = c.refund_tx(op, spk, Amount::from_sat(500)).unwrap();
        assert_eq!(refund.lock_time, absolute::LockTime::from_consensus(1_800_000_000));
        assert_eq!(refund.input[0].sequence, Sequence::ENABLE_LOCKTIME_NO_RBF);
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
        assert_eq!(preimage_from_witness(w), Some(preimage));

        let mut refund = c.refund_tx(prevout, ScriptBuf::from(vec![0u8; 22]), Amount::from_sat(400)).unwrap();
        c.finalize_refund(&mut refund, 0, b"fake-sig", &refund_pk);
        assert_eq!(preimage_from_witness(&refund.input[0].witness), None);

        // The signature (minus the sighash-flag byte) verifies over the unified message.
        let secp = bitcoin::secp256k1::Secp256k1::verification_only();
        let parsed = bitcoin::secp256k1::ecdsa::Signature::from_der(&sig[..sig.len() - 1]).unwrap();
        secp.verify_ecdsa(&Message::from_digest(msg), &parsed, &redeem_pk).unwrap();
    }
}
