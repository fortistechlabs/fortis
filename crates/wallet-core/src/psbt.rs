//! PSBT (BIP-174) support for two related flows that both center on a
//! [`FundingPlan`] a watch-only session already built:
//!
//! - **Air-gapped signing**: a genuinely offline `Session` reviews and signs
//!   it. [`build_unsigned_psbt`] populates exactly the fields
//!   `MasterKey::sign_p2wpkh_tx` needs to re-derive each input's key, and
//!   [`sign_and_finalize_psbt`] calls that same function unchanged — no new
//!   signing primitive.
//! - **Hardware-wallet signing**: an external signer (e.g. a BitBox02) is
//!   handed the same unsigned PSBT and returns it with signatures attached
//!   but not finalized (BIP-174 `partial_sigs`, not final witness data).
//!   [`finalize_externally_signed_psbt`] is the one new primitive this needs
//!   — no private key involved, only re-verification and repackaging of a
//!   signature that already exists.
//!
//! Performs no I/O — this crate has no networking dependency at all, so
//! nothing here can make a network call even by mistake.

use std::collections::BTreeMap;

use bitcoin::bip32::{ChildNumber, Fingerprint, KeySource, Xpub};
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use bitcoin::{Amount, CompressedPublicKey, ScriptBuf, Transaction, TxOut};

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::keys::{parse_path, MasterKey};
use crate::sighash::{sighash_all, SighashVariant};
use crate::wallet::{FundingPlan, Utxo};

fn psbt_err(e: impl std::fmt::Display) -> WalletError {
    WalletError::Bitcoin(e.to_string())
}

/// `(pubkey, key-source)` for `(branch, index)` under `account_xpub` — the
/// same derivation `WalletView::address_at` already uses, public-key-only
/// (this side never touches a private key). `fingerprint` is the *master*
/// key's fingerprint (not the account xpub's own) — that's what a PSBT's
/// `bip32_derivation` origin means, and what the offline signer's private key
/// will match against.
fn derive_bip32(
    account_xpub: &Xpub,
    fingerprint: Fingerprint,
    coin_type: u32,
    account: u32,
    is_change: bool,
    index: u32,
) -> Result<(PublicKey, KeySource)> {
    let secp = Secp256k1::verification_only();
    let branch = u32::from(is_change);
    let child = account_xpub
        .derive_pub(&secp, &[ChildNumber::from_normal_idx(branch)?, ChildNumber::from_normal_idx(index)?])
        .map_err(|e| WalletError::Derivation(e.to_string()))?;
    let path = parse_path(&format!("m/84h/{coin_type}h/{account}h/{branch}/{index}"))?;
    Ok((child.public_key, (fingerprint, path)))
}

/// Build an unsigned PSBT from a plan a watch-only session already produced
/// via `plan_payment`/`plan_sweep` — no new coin selection, no new network
/// call. `account_xpub`/`master_fingerprint` are the account xpub this plan
/// was built against and its master's fingerprint (the binding layer holds
/// both; a watch-only import that never captured a fingerprint simply can't
/// call this).
pub fn build_unsigned_psbt(
    params: &ChainParams,
    account_xpub: &Xpub,
    account: u32,
    master_fingerprint: Fingerprint,
    plan: &FundingPlan,
) -> Result<Psbt> {
    let mut psbt = Psbt::from_unsigned_tx(plan.tx.clone()).map_err(psbt_err)?;
    if plan.selected.len() != psbt.inputs.len() {
        return Err(WalletError::InvalidInput("plan.selected does not match tx.input 1:1".into()));
    }
    for (input, u) in psbt.inputs.iter_mut().zip(&plan.selected) {
        input.witness_utxo = Some(TxOut { value: u.value, script_pubkey: u.script_pubkey.clone() });
        let (pk, source) = derive_bip32(
            account_xpub,
            master_fingerprint,
            params.bip44_coin_type,
            account,
            u.is_change,
            u.derivation_index,
        )?;
        input.bip32_derivation.insert(pk, source);
    }
    if let Some(idx) = plan.change_derivation_index {
        let last = psbt.outputs.len().checked_sub(1).ok_or_else(|| {
            WalletError::InvalidInput("plan has a change index but no outputs".into())
        })?;
        let (pk, source) =
            derive_bip32(account_xpub, master_fingerprint, params.bip44_coin_type, account, true, idx)?;
        psbt.outputs[last].bip32_derivation.insert(pk, source);
    }
    Ok(psbt)
}

/// One output an imported PSBT pays that isn't recognised as this wallet's own
/// change — i.e. something to show the user before they sign.
pub struct PsbtDestination {
    pub script_pubkey: ScriptBuf,
    pub value: Amount,
}

/// Everything the offline signer's confirm screen needs, built entirely from
/// what's embedded in the PSBT — deliberately the same shape as
/// [`FundingPlan`] so the existing confirm UI can render it with minimal
/// changes. Never trusts anything the online side merely *claims* (e.g. "this
/// output is the service fee") — every classification below is re-derived
/// from the PSBT's own key-origin data.
pub struct PsbtReview {
    pub fee: Amount,
    pub change: Option<Amount>,
    pub destinations: Vec<PsbtDestination>,
    /// `OP_RETURN` data, if one of the outputs carries it (opt-in replay protection).
    pub op_return: Option<Vec<u8>>,
    /// Same shape `FundingPlan.selected` uses — one entry per input, in order.
    pub selected: Vec<Utxo>,
}

struct Attributed {
    review: PsbtReview,
    tx: Transaction,
    prevouts: Vec<TxOut>,
    paths: Vec<(bool, u32)>,
}

/// `bip32_derivation` (of an input or an output) → `(is_change, index)` iff
/// some entry both (a) carries `master_key`'s own fingerprint and (b)
/// re-derives, from `master_key`'s *private* key at that entry's path, to the
/// exact pubkey whose P2WPKH script matches `owner`'s scriptPubKey — `None`
/// otherwise. (a) alone would just turn a wrong-device mistake into a clear
/// early error; (b) is the check that's actually load-bearing, and it's the
/// same one `sign_p2wpkh_tx` already makes before it ever signs anything.
/// Shared by inputs and outputs — an "own" change output is attributed
/// exactly the same structural way an "own" input is, never by a label.
fn attribute_one(
    params: &ChainParams,
    master_key: &MasterKey,
    account: u32,
    derivation: &BTreeMap<PublicKey, KeySource>,
    owner: &TxOut,
) -> Option<(bool, u32)> {
    let my_fp = master_key.master_fingerprint();
    let expected_prefix = parse_path(&format!("m/84h/{}h/{account}h", params.bip44_coin_type)).ok()?;
    let expected_prefix = expected_prefix.as_ref();
    for (origin_fp, path) in derivation.values() {
        if *origin_fp != my_fp {
            continue;
        }
        let comps = path.as_ref();
        if comps.len() != 5 || comps[..3] != expected_prefix[..3] {
            continue;
        }
        let is_change = u32::from(comps[3]) == 1;
        let index = u32::from(comps[4]);
        let Ok(pk) = master_key.wallet_pubkey(params, account, is_change, index) else { continue };
        let expected_spk = ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash());
        if expected_spk == owner.script_pubkey {
            return Some((is_change, index));
        }
    }
    None
}

/// Parse `psbt_base64`, confirm every input is attributable to `master_key`
/// (rejecting the whole PSBT otherwise — see [`attribute_one`]), and classify
/// every output (own change / a destination / an `OP_RETURN`).
fn attribute(params: &ChainParams, master_key: &MasterKey, account: u32, psbt_base64: &str) -> Result<Attributed> {
    let psbt: Psbt = psbt_base64.trim().parse().map_err(psbt_err)?;
    let tx = psbt.unsigned_tx.clone();
    if psbt.inputs.len() != tx.input.len() || psbt.outputs.len() != tx.output.len() {
        return Err(WalletError::InvalidInput("psbt input/output count does not match its unsigned_tx".into()));
    }

    let mut selected = Vec::with_capacity(psbt.inputs.len());
    let mut prevouts = Vec::with_capacity(psbt.inputs.len());
    let mut paths = Vec::with_capacity(psbt.inputs.len());
    let mut fee_in = Amount::ZERO;

    for (i, input) in psbt.inputs.iter().enumerate() {
        let utxo = input.witness_utxo.as_ref().ok_or_else(|| {
            WalletError::InvalidInput(format!("input {i}: no witness_utxo (only P2WPKH is supported)"))
        })?;
        let (is_change, index) = attribute_one(params, master_key, account, &input.bip32_derivation, utxo)
            .ok_or_else(|| {
                WalletError::InvalidInput(format!(
                    "input {i}: not attributable to this wallet — wrong device, or a tampered/foreign PSBT"
                ))
            })?;
        fee_in = fee_in
            .checked_add(utxo.value)
            .ok_or_else(|| WalletError::InvalidInput("input value overflows".into()))?;
        prevouts.push(utxo.clone());
        paths.push((is_change, index));
        selected.push(Utxo {
            outpoint: tx.input[i].previous_output,
            value: utxo.value,
            script_pubkey: utxo.script_pubkey.clone(),
            confirmations: 0, // unknown offline — never shown, never relied on
            derivation_index: index,
            is_change,
        });
    }

    let mut fee_out = Amount::ZERO;
    let mut change: Option<Amount> = None;
    let mut op_return = None;
    let mut destinations = Vec::new();
    for (i, output) in psbt.outputs.iter().enumerate() {
        let txout = &tx.output[i];
        fee_out = fee_out
            .checked_add(txout.value)
            .ok_or_else(|| WalletError::InvalidInput("output value overflows".into()))?;
        if txout.script_pubkey.is_op_return() {
            // OP_RETURN itself, then one push — correctly handles a push long
            // enough to need OP_PUSHDATA1 (this wallet's replay-protection
            // payload is ~100 bytes, past the 75-byte direct-push limit), unlike
            // naively slicing a fixed number of leading bytes off the raw script.
            let data = txout
                .script_pubkey
                .instructions()
                .nth(1)
                .and_then(|r| r.ok())
                .and_then(|instr| instr.push_bytes().map(|b| b.as_bytes().to_vec()))
                .unwrap_or_default();
            op_return = Some(data);
            continue;
        }
        if attribute_one(params, master_key, account, &output.bip32_derivation, txout).is_some() {
            change = Some(change.unwrap_or(Amount::ZERO) + txout.value);
            continue;
        }
        destinations.push(PsbtDestination { script_pubkey: txout.script_pubkey.clone(), value: txout.value });
    }

    let fee = fee_in
        .checked_sub(fee_out)
        .ok_or_else(|| WalletError::InvalidInput("outputs spend more than the inputs provide".into()))?;

    Ok(Attributed { review: PsbtReview { fee, change, destinations, op_return, selected }, tx, prevouts, paths })
}

/// Review an imported unsigned PSBT — 100% local, no node/network access, safe
/// to call while genuinely offline.
pub fn review_unsigned_psbt(
    params: &ChainParams,
    master_key: &MasterKey,
    account: u32,
    psbt_base64: &str,
) -> Result<PsbtReview> {
    Ok(attribute(params, master_key, account, psbt_base64)?.review)
}

/// Sign every input of an imported unsigned PSBT and return the finalized,
/// broadcast-ready transaction. Delegates the actual signing to the existing,
/// already-tested `MasterKey::sign_p2wpkh_tx` — this function's only job is
/// turning a PSBT into that function's `(tx, prevouts, paths)` inputs.
pub fn sign_and_finalize_psbt(
    params: &ChainParams,
    master_key: &MasterKey,
    account: u32,
    psbt_base64: &str,
) -> Result<Transaction> {
    let Attributed { mut tx, prevouts, paths, .. } = attribute(params, master_key, account, psbt_base64)?;
    master_key.sign_p2wpkh_tx(params, account, &mut tx, &prevouts, &paths)?;
    Ok(tx)
}

/// Finalize a PSBT that already carries one valid signature per input (as
/// returned by a hardware signer — e.g. a BitBox02's `btcSignPSBT` inserts
/// BIP-174 `partial_sigs`, not final witness data) into a broadcast-ready
/// transaction.
///
/// Unlike [`sign_and_finalize_psbt`], this takes no `MasterKey` and touches
/// no private key at all — it only repackages a signature that already
/// exists. It still verifies every signature cryptographically before
/// trusting it: a hardware signer's firmware is a separate, unaudited-by-us
/// codebase, and broadcasting on faith would turn a firmware bug into a
/// mystifying "node rejected tx" instead of a clear local error — the same
/// "never trust what's embedded, always re-derive it" posture
/// [`attribute_one`] already applies on the review side.
///
/// P2WPKH-only, matching this wallet's whole signing surface: each input
/// must carry exactly one `partial_sigs` entry. Rejects outright, before
/// inspecting any signature, if `params.require_unified_sighash` — no
/// off-the-shelf hardware signer's firmware computes the BLAKE2b chain's
/// `SIGHASH_UNIFIED` message (see `crate::sighash`'s module doc: it's a
/// structurally different tagged hash, not BIP-143 with a different flag
/// byte), so a signature from one is never valid there and must never be
/// accepted as if it were.
pub fn finalize_externally_signed_psbt(params: &ChainParams, psbt_base64: &str) -> Result<Transaction> {
    if params.require_unified_sighash {
        return Err(WalletError::InvalidInput(
            "hardware-wallet signing is only supported on Bitcoin, not this chain".into(),
        ));
    }
    let psbt: Psbt = psbt_base64.trim().parse().map_err(psbt_err)?;
    let mut tx = psbt.unsigned_tx.clone();
    if psbt.inputs.len() != tx.input.len() {
        return Err(WalletError::InvalidInput("psbt input count does not match its unsigned_tx".into()));
    }

    let prevouts: Vec<TxOut> = psbt
        .inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            input
                .witness_utxo
                .clone()
                .ok_or_else(|| WalletError::InvalidInput(format!("input {i}: no witness_utxo")))
        })
        .collect::<Result<_>>()?;

    let secp = Secp256k1::verification_only();
    let mut witnesses = Vec::with_capacity(tx.input.len());
    for (i, input) in psbt.inputs.iter().enumerate() {
        if input.partial_sigs.len() != 1 {
            return Err(WalletError::InvalidInput(format!(
                "input {i}: expected exactly one signature (single-sig P2WPKH only), got {}",
                input.partial_sigs.len()
            )));
        }
        let (pubkey, sig) = input.partial_sigs.iter().next().expect("checked len == 1 above");
        let compressed = CompressedPublicKey::try_from(*pubkey)
            .map_err(|_| WalletError::InvalidInput(format!("input {i}: signing key is not compressed")))?;
        let expected_spk = ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash());
        if expected_spk != prevouts[i].script_pubkey {
            return Err(WalletError::InvalidInput(format!(
                "input {i}: signature's public key does not match this input's spend script"
            )));
        }
        // BIP-143 scriptCode for a P2WPKH input is the implied P2PKH script —
        // the same construction MasterKey::sign_p2wpkh_tx uses when it signs.
        let script_code = ScriptBuf::new_p2pkh(&pubkey.pubkey_hash());
        let sh = sighash_all(&tx, &prevouts, i, &script_code, SighashVariant::SegwitV0)?;
        secp.verify_ecdsa(&Message::from_digest(sh.message), &sig.signature, &pubkey.inner)
            .map_err(|_| WalletError::InvalidInput(format!("input {i}: signature does not verify")))?;

        let mut w = bitcoin::Witness::new();
        w.push(sig.serialize()); // DER + sighash-type byte, same shape sign_p2wpkh_tx produces
        w.push(pubkey.inner.serialize());
        witnesses.push(w);
    }
    for (input, witness) in tx.input.iter_mut().zip(witnesses) {
        input.witness = witness;
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{ServiceFee, WalletView};
    use bitcoin::hashes::Hash;
    use bitcoin::{OutPoint, Txid};

    fn seeded(seed: u8) -> (MasterKey, ChainParams) {
        let (_m, key) = MasterKey::generate(&[seed; 32]).unwrap();
        (key, ChainParams::bitcoin())
    }

    fn funded_view(key: &MasterKey, params: &ChainParams) -> WalletView {
        WalletView::new(params.clone(), key.account_xpub(params, 0).unwrap())
    }

    fn utxo_at(view: &WalletView, branch: u32, index: u32, sats: u64, tag: u8) -> Utxo {
        Utxo {
            outpoint: OutPoint::new(Txid::from_byte_array([tag; 32]), 0),
            value: Amount::from_sat(sats),
            script_pubkey: view.address_at(branch, index).unwrap().script_pubkey(),
            confirmations: 6,
            derivation_index: index,
            is_change: branch == 1,
        }
    }

    /// A plausible P2WPKH-shaped external destination script — not derived
    /// from either test wallet's xpub, so `attribute_one` correctly never
    /// matches it.
    fn dest() -> ScriptBuf {
        let mut v = vec![0x00u8, 0x14];
        v.extend([0xABu8; 20]);
        ScriptBuf::from(v)
    }

    /// The PSBT round trip must produce a byte-identical signed transaction to
    /// calling `sign_p2wpkh_tx` directly on the same plan — the strongest
    /// guarantee this isn't a parallel, divergent implementation.
    #[test]
    fn psbt_round_trip_matches_direct_signing_exactly() {
        let (key, params) = seeded(7);
        let mut view = funded_view(&key, &params);
        let u = utxo_at(&view, 0, 3, 500_000, 0xaa);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        assert!(plan.change.is_some(), "test fixture should produce change");

        let psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        let b64 = psbt.to_string();

        let review = review_unsigned_psbt(&params, &key, 0, &b64).unwrap();
        assert_eq!(review.fee, plan.fee);
        assert_eq!(review.change, plan.change);
        assert_eq!(review.destinations.len(), 1);
        assert_eq!(review.destinations[0].value, Amount::from_sat(400_000));

        let via_psbt = sign_and_finalize_psbt(&params, &key, 0, &b64).unwrap();

        let mut direct = plan.tx.clone();
        let prevouts =
            vec![TxOut { value: plan.selected[0].value, script_pubkey: plan.selected[0].script_pubkey.clone() }];
        key.sign_p2wpkh_tx(&params, 0, &mut direct, &prevouts, &[(false, 3)]).unwrap();

        assert_eq!(
            bitcoin::consensus::encode::serialize(&via_psbt),
            bitcoin::consensus::encode::serialize(&direct),
        );
    }

    #[test]
    fn the_recorded_change_index_is_the_one_actually_used() {
        let (key, params) = seeded(9);
        let mut view = funded_view(&key, &params);
        // Advance next_change past 0 so a wrong "index - 1"-style off-by-one
        // would be caught (index 0 would slip past that bug undetected).
        let _ = view.next_change_address().unwrap();
        let _ = view.next_change_address().unwrap();
        let u = utxo_at(&view, 0, 0, 1_000_000, 0xbb);
        let out = TxOut { value: Amount::from_sat(100_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let idx = plan.change_derivation_index.expect("this plan should have change");
        let actual_change_spk = plan.tx.output.last().unwrap().script_pubkey.clone();
        assert_eq!(view.address_at(1, idx).unwrap().script_pubkey(), actual_change_spk);

        let psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        let review = review_unsigned_psbt(&params, &key, 0, &psbt.to_string()).unwrap();
        assert_eq!(review.change, plan.change);
    }

    #[test]
    fn a_psbt_from_a_different_wallet_is_rejected_outright() {
        let (key_a, params) = seeded(1);
        let (key_b, _) = seeded(2);
        let mut view_a = funded_view(&key_a, &params);
        let u = utxo_at(&view_a, 0, 0, 500_000, 0xcc);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view_a.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let psbt = build_unsigned_psbt(&params, view_a.xpub(), 0, key_a.master_fingerprint(), &plan).unwrap();
        let b64 = psbt.to_string();

        assert!(review_unsigned_psbt(&params, &key_b, 0, &b64).is_err());
        assert!(sign_and_finalize_psbt(&params, &key_b, 0, &b64).is_err());
    }

    #[test]
    fn a_tampered_witness_utxo_value_is_rejected() {
        let (key, params) = seeded(3);
        let mut view = funded_view(&key, &params);
        let u = utxo_at(&view, 0, 0, 500_000, 0xdd);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let mut psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        // Value tampered, script left alone — attribution is script-based, so
        // this doesn't break attribution, but it must not silently pass
        // through into a wrong fee calculation either.
        let original_fee = plan.fee;
        psbt.inputs[0].witness_utxo.as_mut().unwrap().value = Amount::from_sat(999_999_999);
        let review = review_unsigned_psbt(&params, &key, 0, &psbt.to_string()).unwrap();
        assert_ne!(review.fee, original_fee, "a tampered input value must change the computed fee, not be ignored");
    }

    #[test]
    fn missing_witness_utxo_is_rejected() {
        let (key, params) = seeded(4);
        let mut view = funded_view(&key, &params);
        let u = utxo_at(&view, 0, 0, 500_000, 0xee);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let mut psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        psbt.inputs[0].witness_utxo = None;
        assert!(review_unsigned_psbt(&params, &key, 0, &psbt.to_string()).is_err());
    }

    #[test]
    fn multi_input_plan_round_trips_with_correct_per_input_paths() {
        let (key, params) = seeded(5);
        let mut view = funded_view(&key, &params);
        let u1 = utxo_at(&view, 0, 0, 300_000, 0x11);
        let u2 = utxo_at(&view, 0, 1, 300_000, 0x22);
        let u3 = utxo_at(&view, 1, 0, 300_000, 0x33); // an earlier change coin, now being spent
        let out = TxOut { value: Amount::from_sat(700_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u1, u2, u3], vec![out], 5, 1, None, false).unwrap();
        assert_eq!(plan.selected.len(), 3);

        let psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        let signed = sign_and_finalize_psbt(&params, &key, 0, &psbt.to_string()).unwrap();
        for input in &signed.input {
            assert!(!input.witness.is_empty(), "every input must be signed");
        }
    }

    #[test]
    fn cross_chain_sighash_flag_differs_via_the_existing_path() {
        let (key, _btc) = seeded(6);
        let btc = ChainParams::bitcoin();
        let xbt = ChainParams::blake2b();
        for params in [btc, xbt] {
            let mut view = funded_view(&key, &params);
            let u = utxo_at(&view, 0, 0, 500_000, 0xff);
            let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
            let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
            let psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
            let signed = sign_and_finalize_psbt(&params, &key, 0, &psbt.to_string()).unwrap();
            let sig = signed.input[0].witness.to_vec()[0].clone();
            let expected_flag = if params.require_unified_sighash { 0x21 } else { 0x01 };
            assert_eq!(*sig.last().unwrap(), expected_flag);
        }
    }

    #[test]
    fn a_service_fee_output_is_not_mistaken_for_change() {
        let (key, params) = seeded(8);
        let mut view = funded_view(&key, &params);
        let u = utxo_at(&view, 0, 0, 500_000, 0x99);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let sf = ServiceFee { bps: 100, floor_sat: 400, cap_sat: 0, fee_spk: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, Some(&sf), false).unwrap();
        let psbt = build_unsigned_psbt(&params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        let review = review_unsigned_psbt(&params, &key, 0, &psbt.to_string()).unwrap();
        // The real recipient and the fee address happen to share a script in
        // this fixture — two separate destination entries either way, neither
        // one mistaken for change.
        assert_eq!(review.change, plan.change);
        let total_dest: u64 = review.destinations.iter().map(|d| d.value.to_sat()).sum();
        let plan_dest_total = plan.tx.output.iter()
            .filter(|o| !o.script_pubkey.is_op_return() && Some(o.value) != plan.change)
            .map(|o| o.value.to_sat())
            .sum::<u64>();
        assert_eq!(total_dest, plan_dest_total);
    }

    /// Builds a PSBT + a standalone signed copy of the same plan, then lifts
    /// `(pubkey, signature)` out of the signed copy's witness — standing in
    /// for what a hardware signer's `partial_sigs` response contains, without
    /// needing a real device.
    fn psbt_with_external_signature(key: &MasterKey, params: &ChainParams, view: &mut WalletView) -> (Psbt, Transaction) {
        let u = utxo_at(view, 0, 0, 500_000, 0x42);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let mut psbt = build_unsigned_psbt(params, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();

        let mut direct = plan.tx.clone();
        let prevouts =
            vec![TxOut { value: plan.selected[0].value, script_pubkey: plan.selected[0].script_pubkey.clone() }];
        key.sign_p2wpkh_tx(params, 0, &mut direct, &prevouts, &[(false, 0)]).unwrap();
        let witness = direct.input[0].witness.to_vec();
        let sig = bitcoin::ecdsa::Signature::from_slice(&witness[0]).unwrap();
        let pubkey = bitcoin::PublicKey::from_slice(&witness[1]).unwrap();
        psbt.inputs[0].partial_sigs.insert(pubkey, sig);
        (psbt, direct)
    }

    #[test]
    fn finalize_externally_signed_psbt_matches_direct_signing_exactly() {
        let (key, params) = seeded(10);
        let mut view = funded_view(&key, &params);
        let (psbt, direct) = psbt_with_external_signature(&key, &params, &mut view);

        let finalized = finalize_externally_signed_psbt(&params, &psbt.to_string()).unwrap();
        assert_eq!(
            bitcoin::consensus::encode::serialize(&finalized),
            bitcoin::consensus::encode::serialize(&direct),
        );
    }

    #[test]
    fn finalize_externally_signed_psbt_rejects_a_tampered_signature() {
        let (key, params) = seeded(11);
        let mut view = funded_view(&key, &params);
        let (mut psbt, _direct) = psbt_with_external_signature(&key, &params, &mut view);

        let (pubkey, mut sig) = psbt.inputs[0].partial_sigs.iter().next().map(|(k, v)| (*k, *v)).unwrap();
        // Corrupt the DER bytes so the signature no longer verifies against
        // the sighash it's supposed to cover — a stand-in for a tampered or
        // wrong-transaction response from a compromised/buggy signer.
        let mut der = sig.signature.serialize_der().to_vec();
        der[10] ^= 0xFF;
        sig.signature = bitcoin::secp256k1::ecdsa::Signature::from_der(&der)
            .unwrap_or_else(|_| sig.signature); // if corruption made it unparsable, the original still won't verify below
        psbt.inputs[0].partial_sigs.insert(pubkey, sig);

        assert!(finalize_externally_signed_psbt(&params, &psbt.to_string()).is_err());
    }

    #[test]
    fn finalize_externally_signed_psbt_rejects_zero_or_multiple_partial_sigs() {
        let (key, params) = seeded(12);
        let mut view = funded_view(&key, &params);
        let (mut psbt, _direct) = psbt_with_external_signature(&key, &params, &mut view);

        let (pubkey, sig) = psbt.inputs[0].partial_sigs.iter().next().map(|(k, v)| (*k, *v)).unwrap();

        // Zero signatures.
        psbt.inputs[0].partial_sigs.clear();
        assert!(finalize_externally_signed_psbt(&params, &psbt.to_string()).is_err());

        // Two signatures on the same input (a different pubkey, arbitrary — the
        // second entry alone is enough to trip the "exactly one" check before
        // either is even inspected).
        psbt.inputs[0].partial_sigs.insert(pubkey, sig);
        let (key2, _) = seeded(13);
        let other_pubkey = bitcoin::PublicKey::new(key2.account_xpub(&params, 0).unwrap().public_key);
        psbt.inputs[0].partial_sigs.insert(other_pubkey, sig);
        assert!(finalize_externally_signed_psbt(&params, &psbt.to_string()).is_err());
    }

    #[test]
    fn finalize_externally_signed_psbt_refuses_the_blake2b_chain() {
        // No signature needed: finalize_externally_signed_psbt checks
        // require_unified_sighash before it ever parses a signature, so an
        // unsigned PSBT is enough to exercise the rejection. (A real
        // partial_sig can't even be constructed here the normal way: this
        // chain's witness signatures carry a non-standard trailing sighash
        // byte that bitcoin::ecdsa::Signature::from_slice won't parse.)
        let (key, _btc) = seeded(14);
        let xbt = ChainParams::blake2b();
        let mut view = funded_view(&key, &xbt);
        let u = utxo_at(&view, 0, 0, 500_000, 0x42);
        let out = TxOut { value: Amount::from_sat(400_000), script_pubkey: dest() };
        let plan = view.plan_payment(&[u], vec![out], 5, 1, None, false).unwrap();
        let psbt = build_unsigned_psbt(&xbt, view.xpub(), 0, key.master_fingerprint(), &plan).unwrap();
        let err = finalize_externally_signed_psbt(&xbt, &psbt.to_string()).unwrap_err();
        assert!(err.to_string().contains("Bitcoin"), "unexpected error: {err}");
    }
}
