//! Lightning transactions are exempt from the service fee (`pricing.rs`).
//!
//! The wallet's embedded Lightning node (ldk-node) broadcasts through
//! `POST /btc/tx` like everything else, but it cannot add a fee output: it
//! builds its own transactions (channel opens, closes, force closes, anchor
//! fee bumps, HTLC claims, sweeps). Those would all be refused. So a
//! transaction that does not pay the fee is let through when **every** input
//! is one of:
//!
//! 1. a spend of a Lightning contract output: the input's witness script is
//!    one of the BOLT 3 templates (funding 2-of-2, `to_local` and HTLC-output
//!    revokeable script, anchors `to_remote`, keyed anchor, offered / received
//!    HTLC with or without anchors);
//! 2. a coin whose creating transaction qualifies (has a Lightning input, or
//!    paid the service fee, or — up to [`MAX_DEPTH`] hops — has only
//!    qualifying inputs itself): the balance a close paid to the wallet,
//!    change of a fee bump, change of a channel open;
//! 3. a coin created by a transaction that paid the service fee: a
//!    "Move to Lightning" deposit (which paid 1 % when it was sent), spent by
//!    the channel open or swept back when no channel opened.
//!
//! Point 3 means a coin one or two hops downstream of a fee-paying
//! transaction (e.g. its change) can be spent without paying again. The
//! Fortis app always adds the fee to ordinary sends regardless, so only a
//! modified client gains, and such a client could broadcast elsewhere anyway:
//! the edge's fee check is a gate for its own clients, not a consensus rule.

use std::collections::HashMap;

use bitcoin::blockdata::opcodes::all as op;
use bitcoin::script::Instruction;
use bitcoin::{Script, Transaction, Txid, Witness};

/// Hops followed back through non-Lightning, non-fee-paying parents.
pub const MAX_DEPTH: u32 = 2;
/// A transaction with more inputs than this is not checked (and so not exempt).
pub const MAX_INPUTS: usize = 25;
/// Most parent transactions fetched for one check.
pub const MAX_FETCHES: usize = 60;

/// One element of a script template.
#[derive(Clone, Copy)]
enum T {
    Op(u8),
    /// A data push of exactly this many bytes.
    Push(usize),
    /// A number: `OP_1`..`OP_16` or a 1–5 byte push (delays, expiries).
    Num,
}

const fn op(o: bitcoin::Opcode) -> T {
    T::Op(o.to_u8())
}

/// `2 <pk> <pk> 2 CHECKMULTISIG`
const FUNDING: &[T] = &[
    op(op::OP_PUSHNUM_2),
    T::Push(33),
    T::Push(33),
    op(op::OP_PUSHNUM_2),
    op(op::OP_CHECKMULTISIG),
];
/// `<pk> CHECKSIG IFDUP NOTIF 16 CSV ENDIF`
const KEYED_ANCHOR: &[T] = &[
    T::Push(33),
    op(op::OP_CHECKSIG),
    op(op::OP_IFDUP),
    op(op::OP_NOTIF),
    op(op::OP_PUSHNUM_16),
    op(op::OP_CSV),
    op(op::OP_ENDIF),
];
/// Anchors `to_remote`: `<pk> CHECKSIGVERIFY 1 CSV`
const TO_REMOTE_ANCHORS: &[T] = &[
    T::Push(33),
    op(op::OP_CHECKSIGVERIFY),
    op(op::OP_PUSHNUM_1),
    op(op::OP_CSV),
];
/// `to_local` and second-stage HTLC outputs:
/// `IF <revocation> ELSE <delay> CSV DROP <delayed> ENDIF CHECKSIG`
const REVOKEABLE: &[T] = &[
    op(op::OP_IF),
    T::Push(33),
    op(op::OP_ELSE),
    T::Num,
    op(op::OP_CSV),
    op(op::OP_DROP),
    T::Push(33),
    op(op::OP_ENDIF),
    op(op::OP_CHECKSIG),
];
/// Anchor channels end HTLC scripts with `1 CSV DROP` before the last ENDIF.
const ANCHORS_HTLC_SUFFIX: &[T] = &[op(op::OP_PUSHNUM_1), op(op::OP_CSV), op(op::OP_DROP)];
/// Offered HTLC, without its final `[1 CSV DROP] ENDIF`.
const OFFERED_HTLC: &[T] = &[
    op(op::OP_DUP),
    op(op::OP_HASH160),
    T::Push(20),
    op(op::OP_EQUAL),
    op(op::OP_IF),
    op(op::OP_CHECKSIG),
    op(op::OP_ELSE),
    T::Push(33),
    op(op::OP_SWAP),
    op(op::OP_SIZE),
    T::Push(1),
    op(op::OP_EQUAL),
    op(op::OP_NOTIF),
    op(op::OP_DROP),
    op(op::OP_PUSHNUM_2),
    op(op::OP_SWAP),
    T::Push(33),
    op(op::OP_PUSHNUM_2),
    op(op::OP_CHECKMULTISIG),
    op(op::OP_ELSE),
    op(op::OP_HASH160),
    T::Push(20),
    op(op::OP_EQUALVERIFY),
    op(op::OP_CHECKSIG),
    op(op::OP_ENDIF),
];
/// Received HTLC, without its final `[1 CSV DROP] ENDIF`.
const RECEIVED_HTLC: &[T] = &[
    op(op::OP_DUP),
    op(op::OP_HASH160),
    T::Push(20),
    op(op::OP_EQUAL),
    op(op::OP_IF),
    op(op::OP_CHECKSIG),
    op(op::OP_ELSE),
    T::Push(33),
    op(op::OP_SWAP),
    op(op::OP_SIZE),
    T::Push(1),
    op(op::OP_EQUAL),
    op(op::OP_IF),
    op(op::OP_HASH160),
    T::Push(20),
    op(op::OP_EQUALVERIFY),
    op(op::OP_PUSHNUM_2),
    op(op::OP_SWAP),
    T::Push(33),
    op(op::OP_PUSHNUM_2),
    op(op::OP_CHECKMULTISIG),
    op(op::OP_ELSE),
    op(op::OP_DROP),
    T::Num,
    op(op::OP_CLTV),
    op(op::OP_DROP),
    op(op::OP_CHECKSIG),
    op(op::OP_ENDIF),
];

/// Whether `script`'s instructions match `parts` concatenated, exactly.
fn matches(script: &Script, parts: &[&[T]]) -> bool {
    let mut want = parts.iter().flat_map(|p| p.iter());
    for ins in script.instructions() {
        let Ok(ins) = ins else { return false };
        let Some(t) = want.next() else { return false };
        let ok = match (t, ins) {
            (T::Op(o), Instruction::Op(got)) => got.to_u8() == *o,
            (T::Push(n), Instruction::PushBytes(b)) => b.len() == *n,
            (T::Num, Instruction::Op(got)) => {
                (op::OP_PUSHNUM_1.to_u8()..=op::OP_PUSHNUM_16.to_u8()).contains(&got.to_u8())
            }
            (T::Num, Instruction::PushBytes(b)) => (1..=5).contains(&b.len()),
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    want.next().is_none()
}

/// Whether `script` is a BOLT 3 Lightning contract script.
pub fn is_ln_script(script: &Script) -> bool {
    const END: &[T] = &[op(op::OP_ENDIF)];
    [FUNDING, KEYED_ANCHOR, TO_REMOTE_ANCHORS, REVOKEABLE]
        .iter()
        .any(|t| matches(script, &[t]))
        || [OFFERED_HTLC, RECEIVED_HTLC]
            .iter()
            .any(|t| matches(script, &[t, ANCHORS_HTLC_SUFFIX, END]) || matches(script, &[t, END]))
}

/// Whether an input spends a P2WSH output whose witness script (the last
/// witness element) is a Lightning contract script.
pub fn is_ln_spend(witness: &Witness) -> bool {
    witness.len() >= 2
        && witness
            .last()
            .is_some_and(|ws| is_ln_script(Script::from_bytes(ws)))
}

/// Whether `tx`, which pays no service fee, is exempt as a Lightning
/// transaction. `pays_fee` says whether a transaction paid the fee;
/// `fetch` returns a transaction by id (`None`: unknown or unreachable — the
/// coin then does not qualify).
pub fn exempt(
    tx: &Transaction,
    pays_fee: &dyn Fn(&Transaction) -> bool,
    fetch: &mut dyn FnMut(&Txid) -> Option<Transaction>,
) -> bool {
    if tx.input.is_empty() || tx.input.len() > MAX_INPUTS {
        return false;
    }
    let mut ctx = Ctx {
        pays_fee,
        fetch,
        cache: HashMap::new(),
        fetches: 0,
    };
    tx.input
        .iter()
        .all(|i| is_ln_spend(&i.witness) || ctx.coin_qualifies(&i.previous_output.txid, MAX_DEPTH))
}

struct Ctx<'a> {
    pays_fee: &'a dyn Fn(&Transaction) -> bool,
    fetch: &'a mut dyn FnMut(&Txid) -> Option<Transaction>,
    cache: HashMap<Txid, Option<Transaction>>,
    fetches: usize,
}

impl Ctx<'_> {
    fn get(&mut self, txid: &Txid) -> Option<Transaction> {
        if let Some(t) = self.cache.get(txid) {
            return t.clone();
        }
        if self.fetches >= MAX_FETCHES {
            return None;
        }
        self.fetches += 1;
        let t = (self.fetch)(txid);
        self.cache.insert(*txid, t.clone());
        t
    }

    /// A coin created by `txid` qualifies (points 2 and 3 of the module doc).
    fn coin_qualifies(&mut self, txid: &Txid, depth: u32) -> bool {
        let Some(parent) = self.get(txid) else {
            return false;
        };
        if parent.input.iter().any(|i| is_ln_spend(&i.witness)) || (self.pays_fee)(&parent) {
            return true;
        }
        depth > 1
            && !parent.is_coinbase()
            && parent.input.len() <= MAX_INPUTS
            && parent
                .input
                .iter()
                .all(|i| self.coin_qualifies(&i.previous_output.txid, depth - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
    use bitcoin::{
        absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence, TxIn,
        TxOut,
    };
    use lightning::ln::chan_utils::{
        get_htlc_redeemscript, get_keyed_anchor_redeemscript, get_revokeable_redeemscript,
        make_funding_redeemscript, HTLCOutputInCommitment, TxCreationKeys,
    };
    use lightning::ln::channel_keys::{DelayedPaymentKey, HtlcKey, RevocationKey};
    use lightning::types::features::ChannelTypeFeatures;
    use lightning::types::payment::PaymentHash;

    fn pk(n: u8) -> PublicKey {
        PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }

    fn keys() -> TxCreationKeys {
        TxCreationKeys {
            per_commitment_point: pk(1),
            revocation_key: RevocationKey(pk(2)),
            broadcaster_htlc_key: HtlcKey(pk(3)),
            countersignatory_htlc_key: HtlcKey(pk(4)),
            broadcaster_delayed_payment_key: DelayedPaymentKey(pk(5)),
        }
    }

    fn htlc(offered: bool, cltv_expiry: u32) -> HTLCOutputInCommitment {
        HTLCOutputInCommitment {
            offered,
            amount_msat: 1_000_000,
            cltv_expiry,
            payment_hash: PaymentHash([7; 32]),
            transaction_output_index: Some(0),
        }
    }

    /// Every script LDK puts on chain, as LDK itself builds it.
    fn ldk_scripts() -> Vec<(String, ScriptBuf)> {
        let mut v = vec![
            ("funding".into(), make_funding_redeemscript(&pk(1), &pk(2))),
            ("keyed anchor".into(), get_keyed_anchor_redeemscript(&pk(1))),
            // ldk builds this inline in get_countersigner_payment_script.
            (
                "to_remote anchors".into(),
                bitcoin::script::Builder::new()
                    .push_slice(pk(6).serialize())
                    .push_opcode(op::OP_CHECKSIGVERIFY)
                    .push_int(1)
                    .push_opcode(op::OP_CSV)
                    .into_script(),
            ),
        ];
        for delay in [1u16, 16, 17, 144, 2016, 65535] {
            v.push((
                format!("revokeable delay {delay}"),
                get_revokeable_redeemscript(
                    &RevocationKey(pk(2)),
                    delay,
                    &DelayedPaymentKey(pk(5)),
                ),
            ));
        }
        for (name, f) in [
            (
                "anchors",
                ChannelTypeFeatures::anchors_zero_htlc_fee_and_dependencies(),
            ),
            (
                "static_remote_key",
                ChannelTypeFeatures::only_static_remote_key(),
            ),
        ] {
            for cltv in [1u32, 16, 17, 500_000, 974_404, 0x7fff_ffff] {
                for offered in [true, false] {
                    v.push((
                        format!("{name} offered={offered} cltv={cltv}"),
                        get_htlc_redeemscript(&htlc(offered, cltv), &f, &keys()),
                    ));
                }
            }
        }
        v
    }

    #[test]
    fn recognises_every_script_ldk_builds() {
        for (name, s) in ldk_scripts() {
            assert!(is_ln_script(&s), "{name}: {}", s.to_asm_string());
        }
    }

    #[test]
    fn ordinary_scripts_are_not_lightning() {
        let p2wpkh = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::hash(&pk(1).serialize()));
        let multisig_2of3 = bitcoin::script::Builder::new()
            .push_int(2)
            .push_slice(pk(1).serialize())
            .push_slice(pk(2).serialize())
            .push_slice(pk(3).serialize())
            .push_int(3)
            .push_opcode(op::OP_CHECKMULTISIG)
            .into_script();
        let single_key = bitcoin::script::Builder::new()
            .push_slice(pk(1).serialize())
            .push_opcode(op::OP_CHECKSIG)
            .into_script();
        for s in [p2wpkh, multisig_2of3, single_key, ScriptBuf::new()] {
            assert!(!is_ln_script(&s), "{}", s.to_asm_string());
        }
        // A Lightning script with something appended is not one.
        let mut funding = make_funding_redeemscript(&pk(1), &pk(2)).into_bytes();
        funding.push(op::OP_DROP.to_u8());
        assert!(!is_ln_script(Script::from_bytes(&funding)));
    }

    fn input(prev: Txid, witness: Vec<Vec<u8>>) -> TxIn {
        TxIn {
            previous_output: OutPoint {
                txid: prev,
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&witness),
        }
    }

    /// A P2WPKH-style spend (sig, pubkey): not a Lightning script.
    fn wallet_spend(prev: Txid) -> TxIn {
        input(prev, vec![vec![0x30; 72], pk(9).serialize().to_vec()])
    }

    fn ln_spend(prev: Txid) -> TxIn {
        let ws = make_funding_redeemscript(&pk(1), &pk(2)).into_bytes();
        input(prev, vec![vec![], vec![0x30; 72], vec![0x30; 72], ws])
    }

    const FEE_MARK: u64 = 4_242;

    fn tx(inputs: Vec<TxIn>, pays_fee: bool) -> Transaction {
        let mut output = vec![TxOut {
            value: Amount::from_sat(10_000),
            script_pubkey: ScriptBuf::new(),
        }];
        if pays_fee {
            output.push(TxOut {
                value: Amount::from_sat(FEE_MARK),
                script_pubkey: ScriptBuf::new(),
            });
        }
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: inputs,
            output,
        }
    }

    fn pays_fee(t: &Transaction) -> bool {
        t.output.iter().any(|o| o.value.to_sat() == FEE_MARK)
    }

    fn id(n: u8) -> Txid {
        Txid::from_byte_array([n; 32])
    }

    /// Runs `exempt` over a little chain of known transactions.
    fn check(t: &Transaction, known: &[(Txid, Transaction)]) -> bool {
        let map: HashMap<Txid, Transaction> = known.iter().cloned().collect();
        exempt(t, &pays_fee, &mut |txid| map.get(txid).cloned())
    }

    #[test]
    fn a_close_or_force_close_is_exempt_without_any_lookup() {
        let close = tx(vec![ln_spend(id(1))], false);
        assert!(exempt(&close, &pays_fee, &mut |_| panic!(
            "no lookup needed"
        )));
    }

    #[test]
    fn an_anchor_bump_with_a_wallet_input_needs_that_coin_to_qualify() {
        // Anchor spend + a wallet coin from a deposit that paid the fee.
        let deposit = tx(vec![wallet_spend(id(9))], true);
        let bump = tx(vec![ln_spend(id(1)), wallet_spend(id(2))], false);
        assert!(check(&bump, &[(id(2), deposit)]));
        // ...but not with a coin of unknown origin.
        let random = tx(vec![wallet_spend(id(9))], false);
        assert!(!check(&bump, &[(id(2), random)]));
    }

    #[test]
    fn a_channel_open_from_a_fee_paying_deposit_is_exempt() {
        let deposit = tx(vec![wallet_spend(id(9))], true);
        let open = tx(vec![wallet_spend(id(2))], false);
        assert!(check(&open, &[(id(2), deposit)]));
    }

    #[test]
    fn sweeping_a_coop_close_balance_is_exempt() {
        let close = tx(vec![ln_spend(id(1))], false);
        let sweep = tx(vec![wallet_spend(id(2))], false);
        assert!(check(&sweep, &[(id(2), close)]));
    }

    #[test]
    fn spending_channel_open_change_is_exempt_two_hops_back() {
        let deposit = tx(vec![wallet_spend(id(9))], true);
        let open = tx(vec![wallet_spend(id(3))], false); // its change is a wallet coin
        let sweep = tx(vec![wallet_spend(id(2))], false);
        assert!(check(
            &sweep,
            &[(id(2), open.clone()), (id(3), deposit.clone())]
        ));
        // Three hops back is too far.
        let deeper = tx(vec![wallet_spend(id(4))], false);
        assert!(!check(
            &deeper,
            &[(id(4), sweep), (id(2), open), (id(3), deposit)]
        ));
    }

    #[test]
    fn an_ordinary_send_is_not_exempt() {
        let parent = tx(vec![wallet_spend(id(9))], false);
        let send = tx(vec![wallet_spend(id(2))], false);
        assert!(!check(&send, &[(id(2), parent.clone()), (id(9), parent)]));
        // Unknown parent: not exempt.
        assert!(!check(&send, &[]));
        // One qualifying input is not enough: every input must qualify.
        let mixed = tx(vec![ln_spend(id(1)), wallet_spend(id(5))], false);
        assert!(!check(&mixed, &[]));
        // No inputs at all.
        assert!(!check(&tx(vec![], false), &[]));
    }

    #[test]
    fn too_many_inputs_is_not_checked() {
        let many = tx(
            (0..=MAX_INPUTS as u8).map(|n| ln_spend(id(n))).collect(),
            false,
        );
        assert!(!check(&many, &[]));
    }

    #[test]
    fn lookups_are_bounded() {
        // A long fan-in of non-qualifying parents never fetches more than MAX_FETCHES.
        let mut fetched = 0;
        let parent = tx(
            (0..MAX_INPUTS as u8).map(|n| wallet_spend(id(n))).collect(),
            false,
        );
        let t = tx(
            (0..MAX_INPUTS as u8).map(|n| wallet_spend(id(n))).collect(),
            false,
        );
        exempt(&t, &pays_fee, &mut |_| {
            fetched += 1;
            Some(parent.clone())
        });
        assert!(fetched <= MAX_FETCHES, "{fetched}");
    }
}
