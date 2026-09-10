//! End-to-end swap between two wallets: both HTLC legs, funding + redeem + refund.
//!
//! BTC-side spends are checked against libbitcoinconsensus (real script + CLTV +
//! BIP-143 signature validation). BLAKE2b-side spends use `SIGHASH_UNIFIED`, which
//! stock libbitcoinconsensus does not know, so those are checked for witness
//! structure and signature validity over the unified message.
//!
//! Run with: `cargo test -p wallet-core --features consensus-verify`

#![cfg(feature = "consensus-verify")]

use wallet_core::bitcoin::consensus::encode::serialize;
use wallet_core::bitcoin::hashes::{sha256, Hash};
use wallet_core::bitcoin::secp256k1::{ecdsa, Message, PublicKey, Secp256k1, Verification};
use wallet_core::bitcoin::{
    absolute, Amount, CompressedPublicKey, OutPoint, ScriptBuf, Transaction, TxOut, Txid,
};
use wallet_core::chain::ChainParams;
use wallet_core::htlc::preimage_from_witness;
use wallet_core::keys::MasterKey;
use wallet_core::swap::{SwapMachine, SwapParams, SwapRole};
use wallet_core::wallet::{Utxo, WalletView};
use wallet_core::Chain;

const SWAP: u32 = 5;
const FEE: Amount = Amount::from_sat(600);

fn key(seed: u8) -> MasterKey {
    MasterKey::generate(&[seed; 32]).unwrap().1
}

fn receive_spk(key: &MasterKey, params: &ChainParams) -> ScriptBuf {
    WalletView::new(params.clone(), key.account_xpub(params, 0).unwrap())
        .next_receive_address()
        .unwrap()
        .script_pubkey()
}

/// A spendable P2WPKH coin at `m/84'/coin'/0'/0/index`, plus its prevout.
fn p2wpkh_coin(key: &MasterKey, params: &ChainParams, index: u32, sats: u64, tag: u8) -> (Utxo, TxOut) {
    let pk = key.wallet_pubkey(params, 0, false, index).unwrap();
    let spk = ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash());
    let value = Amount::from_sat(sats);
    (
        Utxo {
            outpoint: OutPoint::new(Txid::from_byte_array([tag; 32]), 0),
            value,
            script_pubkey: spk.clone(),
            confirmations: 20,
            derivation_index: index,
            is_change: false,
        },
        TxOut { value, script_pubkey: spk },
    )
}

fn htlc_vout(tx: &Transaction, spk: &ScriptBuf) -> u32 {
    tx.output.iter().position(|o| &o.script_pubkey == spk).unwrap() as u32
}

fn ecdsa_verifies<C: Verification>(secp: &Secp256k1<C>, sig: &[u8], msg: [u8; 32], pk: &PublicKey) {
    let parsed = ecdsa::Signature::from_der(&sig[..sig.len() - 1]).unwrap();
    secp.verify_ecdsa(&Message::from_digest(msg), &parsed, pk).unwrap();
}

#[test]
fn full_swap_roundtrip() {
    let secp = Secp256k1::new();
    let btc = ChainParams::bitcoin();
    let xbt = ChainParams::blake2b();

    // Alice is the initiator (holds XBT, wants BTC). Bob is the participant.
    let alice = key(0xa1);
    let bob = key(0xb0);

    let preimage = [0x11u8; 32];
    let hashlock = sha256::Hash::hash(&preimage);

    let a_xbt = alice.swap_pubkey(&xbt, 0, SWAP).unwrap();
    let a_btc = alice.swap_pubkey(&btc, 0, SWAP).unwrap();
    let b_btc = bob.swap_pubkey(&btc, 0, SWAP).unwrap();
    let b_xbt = bob.swap_pubkey(&xbt, 0, SWAP).unwrap();

    let send_xbt = Amount::from_sat(2_000_000);
    let send_btc = Amount::from_sat(1_000_000);
    let t_long: u32 = 1_900_000_000;
    let t_short: u32 = 1_850_000_000;

    let mut am = SwapMachine::new(SwapParams {
        swap_id: [1; 16],
        role: SwapRole::Initiator,
        send_chain: Chain::Xbt,
        recv_chain: Chain::Btc,
        send_amount: send_xbt,
        recv_amount: send_btc,
        hashlock,
        our_pubkey_send: a_xbt,
        their_pubkey_send: b_xbt,
        our_pubkey_recv: a_btc,
        their_pubkey_recv: b_btc,
        our_contract_locktime: t_long,
        their_contract_locktime: t_short,
        required_incoming_confs: 6,
    });
    let mut bm = SwapMachine::new(SwapParams {
        swap_id: [1; 16],
        role: SwapRole::Participant,
        send_chain: Chain::Btc,
        recv_chain: Chain::Xbt,
        send_amount: send_btc,
        recv_amount: send_xbt,
        hashlock,
        our_pubkey_send: b_btc,
        their_pubkey_send: a_btc,
        our_pubkey_recv: b_xbt,
        their_pubkey_recv: a_xbt,
        our_contract_locktime: t_short,
        their_contract_locktime: t_long,
        required_incoming_confs: 100,
    });

    let a_htlc = am.build_our_contract(&xbt).unwrap().clone();
    let b_htlc = bm.build_our_contract(&btc).unwrap().clone();

    // ---- Bob funds his BTC HTLC from a P2WPKH coin; check against consensus ----
    let (b_coin, b_prevout) = p2wpkh_coin(&bob, &btc, 0, 1_150_000, 0x01);
    let mut b_view = WalletView::new(btc.clone(), bob.account_xpub(&btc, 0).unwrap());
    let mut b_fund = b_view.plan_htlc_funding(&[b_coin], &b_htlc, 4, 1).unwrap();
    bob.sign_p2wpkh_tx(&btc, 0, &mut b_fund.tx, std::slice::from_ref(&b_prevout), &[(false, 0)]).unwrap();
    b_prevout
        .script_pubkey
        .verify(0, b_prevout.value, &serialize(&b_fund.tx))
        .expect("bob's P2WPKH-funded BTC HTLC funding must pass consensus");
    assert_eq!(b_fund.tx.output.len(), 2); // htlc + change
    let b_funding_txid = b_fund.tx.compute_txid();
    let b_htlc_op = OutPoint::new(b_funding_txid, htlc_vout(&b_fund.tx, &b_htlc.script_pubkey));

    // ---- Alice funds her XBT HTLC (SIGHASH_UNIFIED) ----
    let (a_coin, a_prevout) = p2wpkh_coin(&alice, &xbt, 0, 2_200_000, 0x02);
    let mut a_view = WalletView::new(xbt.clone(), alice.account_xpub(&xbt, 0).unwrap());
    let mut a_fund = a_view.plan_htlc_funding(&[a_coin], &a_htlc, 4, 1).unwrap();
    alice.sign_p2wpkh_tx(&xbt, 0, &mut a_fund.tx, &[a_prevout], &[(false, 0)]).unwrap();
    assert_eq!(a_fund.tx.input[0].witness.nth(0).unwrap().last(), Some(&0x21)); // ALL|UNIFIED
    let a_funding_bytes = serialize(&a_fund.tx);
    let a_funding_txid = a_fund.tx.compute_txid();
    let a_htlc_vout = htlc_vout(&a_fund.tx, &a_htlc.script_pubkey);

    // ---- Each side verifies the other's on-chain contract ----
    let a_op = bm.verify_their_contract(&a_funding_bytes, &xbt).unwrap();
    assert_eq!(a_op, OutPoint::new(a_funding_txid, a_htlc_vout));
    let b_op = am.verify_their_contract(&serialize(&b_fund.tx), &btc).unwrap();
    assert_eq!(b_op, b_htlc_op);

    // ---- Alice redeems Bob's BTC HTLC with the preimage; consensus-verify ----
    let a_redeem = am.build_redeem(b_op, receive_spk(&alice, &btc), FEE).unwrap();
    let a_sig = alice.sign_swap(&btc, 0, SWAP, &a_redeem.sighash).unwrap();
    assert_eq!(a_sig.last(), Some(&0x01));
    let mut a_redeem_tx = a_redeem.tx.clone();
    am.finalize_their_redeem(&mut a_redeem_tx, &a_sig, &a_btc, &preimage).unwrap();
    b_htlc
        .script_pubkey
        .verify(0, b_htlc.value, &serialize(&a_redeem_tx))
        .expect("alice's redeem of bob's BTC HTLC must pass consensus");
    // the watchtower learns the secret from this transaction
    assert_eq!(preimage_from_witness(&a_redeem_tx.input[0].witness), Some(preimage));

    // ---- Bob redeems Alice's XBT HTLC with the now-public preimage (unified) ----
    let b_redeem = bm.build_redeem(a_op, receive_spk(&bob, &xbt), FEE).unwrap();
    let b_sig = bob.sign_swap(&xbt, 0, SWAP, &b_redeem.sighash).unwrap();
    assert_eq!(b_sig.last(), Some(&0x21));
    ecdsa_verifies(&secp, &b_sig, b_redeem.sighash, &b_xbt);
    let mut b_redeem_tx = b_redeem.tx.clone();
    bm.finalize_their_redeem(&mut b_redeem_tx, &b_sig, &b_xbt, &preimage).unwrap();
    assert_eq!(preimage_from_witness(&b_redeem_tx.input[0].witness), Some(preimage));

    // ---- Refund paths ----
    // Bob refunds his own BTC HTLC after t_short: consensus enforces the CLTV.
    let b_refund = bm.build_refund(b_htlc_op, receive_spk(&bob, &btc), FEE).unwrap();
    let b_rsig = bob.sign_swap(&btc, 0, SWAP, &b_refund.sighash).unwrap();
    let mut b_refund_tx = b_refund.tx.clone();
    bm.finalize_our_refund(&mut b_refund_tx, &b_rsig, &b_btc).unwrap();
    assert_eq!(b_refund_tx.lock_time, absolute::LockTime::from_consensus(t_short));
    b_htlc
        .script_pubkey
        .verify(0, b_htlc.value, &serialize(&b_refund_tx))
        .expect("bob's BTC HTLC refund must pass consensus (CLTV path)");

    // Alice refunds her own XBT HTLC after t_long (unified) — structure + signature.
    let a_own_op = OutPoint::new(a_funding_txid, a_htlc_vout);
    let a_refund = am.build_refund(a_own_op, receive_spk(&alice, &xbt), FEE).unwrap();
    let a_rsig = alice.sign_swap(&xbt, 0, SWAP, &a_refund.sighash).unwrap();
    ecdsa_verifies(&secp, &a_rsig, a_refund.sighash, &a_xbt);
    let mut a_refund_tx = a_refund.tx.clone();
    am.finalize_our_refund(&mut a_refund_tx, &a_rsig, &a_xbt).unwrap();
    let w = &a_refund_tx.input[0].witness;
    assert_eq!(w.len(), 4);
    assert!(w.nth(2).unwrap().is_empty()); // refund path: empty branch selector
}
