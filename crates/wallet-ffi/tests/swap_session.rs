//! `SwapSession`: two phones (initiator sends XBT, participant sends BTC) build
//! the same two HTLCs from the same terms and finish a swap with each other.

use std::sync::Arc;

use wallet_core::bitcoin::consensus::encode::deserialize_hex;
use wallet_core::bitcoin::{Transaction, Txid};
use wallet_core::{finish_presigned_redeem, swap_timelocks, ChainParams, PresignedRedeem, SwapTerms};
use wallet_ffi::{participant_fund_latest, swap_commission_sat, txid_of, validate_swap_terms, SwapSession, Wallet};

const ALICE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const BOB: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const MATCHED_AT: u32 = 1_800_000_000;
const SWAP_INDEX: u32 = 7;
const NONCE: &str = "0102030405060708090a0b0c0d0e0f10";

fn terms() -> SwapTerms {
    let tl = swap_timelocks(&ChainParams::bitcoin_regtest(), &ChainParams::blake2b_regtest());
    let price = 1_120_000;
    SwapTerms {
        match_id: "match-1".into(),
        network: "regtest".into(),
        xbt_sat: 10_000_000,
        btc_sat: wallet_core::btc_for_xbt(10_000_000, price),
        price_sat_per_xbt: price,
        fee_bps: 50,
        fee_address: "bcrt1qfee".into(),
        matched_at: MATCHED_AT,
        xbt_locktime: MATCHED_AT + tl.xbt_leg_secs,
        btc_locktime: MATCHED_AT + tl.btc_leg_secs,
        xbt_confs: 6,
        btc_confs: 2,
        accept_by: MATCHED_AT + 120,
        initiator_fund_by: MATCHED_AT + 1920,
        contract_version: wallet_core::htlc::CURRENT_CONTRACT,
    }
}

struct Phone {
    xbt_secret: Vec<u8>,
    btc_secret: Vec<u8>,
    address: String,
}

impl Phone {
    fn new(mnemonic: &str) -> Self {
        let w = Wallet::from_mnemonic(mnemonic.into(), String::new(), "regtest".into()).unwrap();
        // The same seed derives the same `bcrt1` address on both chains.
        let view = wallet_ffi::WalletView::new("btc".into(), "regtest".into(), w.account_xpub("btc".into(), 0).unwrap()).unwrap();
        Phone {
            xbt_secret: w.swap_secret("xbt".into(), 0, SWAP_INDEX).unwrap(),
            btc_secret: w.swap_secret("btc".into(), 0, SWAP_INDEX).unwrap(),
            address: view.address_at(0, 0).unwrap().address,
        }
    }

    fn session(&self, role: &str) -> Arc<SwapSession> {
        self.session_with(role, if role == "initiator" { NONCE } else { "" }).unwrap()
    }

    fn session_with(&self, role: &str, nonce: &str) -> Result<Arc<SwapSession>, wallet_ffi::FfiError> {
        SwapSession::new(
            serde_json::to_string(&terms()).unwrap(),
            role.into(),
            self.xbt_secret.clone(),
            self.btc_secret.clone(),
            self.address.clone(),
            self.address.clone(),
            nonce.into(),
        )
    }
}

/// (alice's initiator session, bob's participant session), keys and hashlock exchanged.
fn matched() -> (Phone, Phone, Arc<SwapSession>, Arc<SwapSession>) {
    let (alice, bob) = (Phone::new(ALICE), Phone::new(BOB));
    let (a, b) = (alice.session("initiator"), bob.session("participant"));
    let hashlock = a.hashlock().unwrap();
    assert_eq!(b.hashlock(), None);
    a.set_counterparty(b.our_pubkeys(), hashlock.clone()).unwrap();
    b.set_counterparty(a.our_pubkeys(), hashlock).unwrap();
    (alice, bob, a, b)
}

fn fake_txid(n: u8) -> String {
    Txid::from_raw_hash(wallet_core::bitcoin::hashes::Hash::from_byte_array([n; 32])).to_string()
}

#[test]
fn initiator_and_participant_agree_on_both_htlcs() {
    let (_alice, _bob, a, b) = matched();
    let t = terms();

    let a_ours = a.our_htlc().unwrap();
    let b_theirs = b.their_htlc().unwrap();
    assert_eq!(a_ours, b_theirs);
    assert_eq!(a_ours.amount_sat, t.xbt_sat);
    assert!(a_ours.address.starts_with("bcrt1q"), "{}", a_ours.address);

    let b_ours = b.our_htlc().unwrap();
    assert_eq!(b_ours, a.their_htlc().unwrap());
    assert_eq!(b_ours.amount_sat, t.btc_sat);
    assert_ne!(a_ours.script_pubkey_hex, b_ours.script_pubkey_hex);
}

#[test]
fn short_or_wrong_script_funding_is_rejected() {
    let (_alice, _bob, a, b) = matched();
    let xbt = a.our_htlc().unwrap();
    let btc = b.our_htlc().unwrap();

    assert!(b.verify_their_funding(xbt.script_pubkey_hex.clone(), xbt.amount_sat - 1).is_err());
    // over-funding would make our redeem sighash (committing to recv_amount) invalid
    assert!(b.verify_their_funding(xbt.script_pubkey_hex.clone(), xbt.amount_sat + 1).is_err());
    assert!(b.verify_their_funding(btc.script_pubkey_hex.clone(), xbt.amount_sat).is_err());
    b.verify_their_funding(xbt.script_pubkey_hex, xbt.amount_sat).unwrap();
}

#[test]
fn session_json_round_trip_needs_the_secrets() {
    let (alice, bob, a, _b) = matched();
    let json = a.to_json();
    assert!(!json.contains(&hex::encode(&alice.xbt_secret)));
    assert!(!json.contains(&hex::encode(&alice.btc_secret)));

    let back = SwapSession::from_json(json.clone(), alice.xbt_secret.clone(), alice.btc_secret.clone()).unwrap();
    assert_eq!(back.our_htlc().unwrap(), a.our_htlc().unwrap());
    assert_eq!(back.preimage(), a.preimage());

    assert!(SwapSession::from_json(json.clone(), bob.xbt_secret.clone(), alice.btc_secret.clone()).is_err());
    assert!(SwapSession::from_json(json, alice.xbt_secret.clone(), bob.btc_secret.clone()).is_err());
}

#[test]
fn new_rejects_a_bad_secret_or_role() {
    let alice = Phone::new(ALICE);
    let t = serde_json::to_string(&terms()).unwrap();
    let mk = |role: &str, xbt: Vec<u8>| {
        SwapSession::new(t.clone(), role.into(), xbt, alice.btc_secret.clone(), alice.address.clone(), alice.address.clone(), NONCE.into())
    };
    assert!(mk("initiator", vec![0u8; 32]).is_err());
    assert!(mk("initiator", vec![1u8; 31]).is_err());
    assert!(mk("maker", alice.xbt_secret.clone()).is_err());
}

#[test]
fn participant_redeems_with_preimage_from_initiator_redeem_tx() {
    let (_alice, _bob, a, b) = matched();
    let (xbt, btc) = (a.our_htlc().unwrap(), b.our_htlc().unwrap());

    // the initiator redeems the participant's BTC HTLC with its own preimage
    a.verify_their_funding(btc.script_pubkey_hex, btc.amount_sat).unwrap();
    a.record_their_outpoint(fake_txid(0xb7), 0).unwrap();
    let preimage = a.preimage().unwrap();
    let redeems = a.signed_redeem(preimage.clone(), vec![2, 20]).unwrap();
    assert_eq!(redeems.len(), 2);

    // the participant learns it from that redeem on chain
    assert_eq!(b.preimage(), None);
    assert_eq!(b.preimage_from_tx(redeems[0].clone()), Some(preimage.clone()));
    assert_eq!(b.preimage_from_tx("00".into()), None);

    // and finishes its redeem of the initiator's XBT HTLC, signed earlier
    b.verify_their_funding(xbt.script_pubkey_hex, xbt.amount_sat).unwrap();
    b.record_their_outpoint(fake_txid(0xa1), 1).unwrap();
    let pre = b.presigned_redeem(vec![2, 20]).unwrap();
    assert_eq!(pre.len(), 2);
    let p = &pre[0];
    let core = PresignedRedeem {
        tx_hex: p.tx_hex.clone(),
        sig_hex: p.sig_hex.clone(),
        pubkey_hex: p.pubkey_hex.clone(),
        witness_script_hex: p.witness_script_hex.clone(),
    };
    let preimage: [u8; 32] = hex::decode(&preimage).unwrap().try_into().unwrap();
    let tx = finish_presigned_redeem(&core, &preimage).unwrap();
    assert_eq!(tx.input[0].witness.len(), 5);
    assert_eq!(tx.input[0].previous_output.txid.to_string(), fake_txid(0xa1));
    assert_eq!(tx.input[0].previous_output.vout, 1);
}

#[test]
fn refund_spends_our_recorded_outpoint() {
    let (_alice, _bob, a, _b) = matched();
    assert!(a.signed_refund(vec![2]).is_err(), "no outpoint recorded yet");
    a.record_our_outpoint(fake_txid(0xa1), 1).unwrap();
    let refunds = a.signed_refund(vec![2, 20]).unwrap();
    assert_eq!(refunds.len(), 2);
    let tx: Transaction = deserialize_hex(&refunds[0]).unwrap();
    assert_eq!(tx.input[0].previous_output.vout, 1);
    assert_eq!(tx.input[0].witness.len(), 5, "a v2 refund");
    assert_eq!(tx.lock_time.to_consensus_u32(), terms().xbt_locktime);
}

/// A swap matched before contract versions (its terms carry none) is a v1
/// swap: it builds the v1 script and refunds with the v1 witness — the coins
/// already locked in such a contract stay refundable. It has no release.
#[test]
fn a_swap_from_before_versions_keeps_the_v1_contract() {
    let alice = Phone::new(ALICE);
    let mut t = serde_json::to_value(terms()).unwrap();
    t.as_object_mut().unwrap().remove("contract_version");
    let old = SwapSession::new(
        t.to_string(), "initiator".into(), alice.xbt_secret.clone(), alice.btc_secret.clone(),
        alice.address.clone(), alice.address.clone(), NONCE.into(),
    )
    .unwrap();
    let bob = Phone::new(BOB);
    let b = bob.session("participant");
    old.set_counterparty(b.our_pubkeys(), old.hashlock().unwrap()).unwrap();
    old.record_our_outpoint(fake_txid(0xa1), 1).unwrap();
    let tx: Transaction = deserialize_hex(&old.signed_refund(vec![2]).unwrap()[0]).unwrap();
    assert_eq!(tx.input[0].witness.len(), 4, "the v1 refund witness");
    assert!(old.release_txs(vec![2]).is_err());
    assert_ne!(old.our_htlc().unwrap().script_pubkey_hex, {
        let a = alice.session("initiator");
        a.set_counterparty(b.our_pubkeys(), a.hashlock().unwrap()).unwrap();
        a.our_htlc().unwrap().script_pubkey_hex
    });
}

#[test]
fn on_event_advances_and_survives_json() {
    let (alice, _bob, a, _b) = matched();
    let s = a.on_event(format!(r#"{{"type":"OurFundingBroadcast","txid":"{}"}}"#, fake_txid(1))).unwrap();
    assert!(s.contains("OurFundingBroadcast"), "{s}");
    let s = a.on_event(r#"{"type":"CounterpartyFunded","confirmations":2}"#.into()).unwrap();
    assert!(s.contains("CounterpartyFundingConfirmed"), "{s}");
    assert!(a.on_event(r#"{"type":"Nope"}"#.into()).is_err());

    let back = SwapSession::from_json(a.to_json(), alice.xbt_secret, alice.btc_secret).unwrap();
    let s = back.on_event(r#"{"type":"TimelockExpired"}"#.into()).unwrap();
    assert!(s.contains("CounterpartyFundingConfirmed"), "{s}");
}

#[test]
fn participant_fund_latest_keeps_half_a_short_btc_leg() {
    let t = terms();
    let json = serde_json::to_string(&t).unwrap();
    let leg = t.btc_locktime - t.matched_at;
    assert_eq!(participant_fund_latest(json).unwrap(), t.btc_locktime - (12 * 3600).min(leg / 2));
    assert!(participant_fund_latest("{}".into()).is_err());
}

#[test]
fn swap_commission_sat_matches_core() {
    assert_eq!(swap_commission_sat(79_999), 400);
    assert_eq!(swap_commission_sat(200_000), 1_000);
}

#[test]
fn validate_swap_terms_checks_limit_and_role() {
    let t = terms();
    let json = serde_json::to_string(&t).unwrap();
    let now = MATCHED_AT + 10;
    let price = t.price_sat_per_xbt;
    let net = || "regtest".to_string();
    assert!(validate_swap_terms(json.clone(), price, "participant".into(), now, net()).is_ok());
    assert!(validate_swap_terms(json.clone(), price - 1, "participant".into(), now, net()).is_err());
    assert!(validate_swap_terms(json.clone(), price + 1, "initiator".into(), now, net()).is_err());
    assert!(validate_swap_terms(json.clone(), price, "taker".into(), now, net()).is_err());
    assert!(validate_swap_terms(json.clone(), price, "initiator".into(), t.accept_by + 1, net()).is_err());
    // Regtest terms on a mainnet phone: refused, whatever else they say.
    assert!(validate_swap_terms(json, price, "participant".into(), now, "bitcoin".into()).is_err());
}

#[test]
fn txid_of_a_signed_segwit_tx_ignores_the_witness() {
    let (_alice, _bob, a, _b) = matched();
    a.record_our_outpoint(fake_txid(0xa1), 1).unwrap();
    let hex = a.signed_refund(vec![2]).unwrap().remove(0);
    let tx: Transaction = deserialize_hex(&hex).unwrap();
    assert_eq!(txid_of(hex).unwrap(), tx.compute_txid().to_string());
    assert!(txid_of("zz".into()).is_err());
}

/// A counterparty claiming our own keys is refused: the two contracts would
/// collapse into one key, and it is never an honest peer.
#[test]
fn our_own_keys_as_the_counterparty_are_refused() {
    let alice = Phone::new(ALICE);
    let a = alice.session("initiator");
    let hashlock = a.hashlock().unwrap();
    assert!(a.set_counterparty(a.our_pubkeys(), hashlock.clone()).is_err());
    let bob = Phone::new(BOB);
    let mixed = wallet_ffi::SwapPubkeys { xbt: a.our_pubkeys().xbt, btc: bob.session("participant").our_pubkeys().btc };
    assert!(a.set_counterparty(mixed, hashlock.clone()).is_err(), "either key equal to ours is refused");
    assert!(a.set_counterparty(bob.session("participant").our_pubkeys(), hashlock).is_ok());
}

/// A swap's recovery tag: the same for the same seed, account and index, so
/// a restored phone can derive it again; different per index and account;
/// and nothing like the swap's own keys.
#[test]
fn recovery_tag_is_deterministic_and_unlinkable() {
    let w = Wallet::from_mnemonic(ALICE.into(), String::new(), "regtest".into()).unwrap();
    let again = Wallet::from_mnemonic(ALICE.into(), String::new(), "regtest".into()).unwrap();
    let t0 = w.swap_recovery_tag(0, 0).unwrap();
    assert_eq!(t0.len(), 64);
    assert!(t0.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(t0, again.swap_recovery_tag(0, 0).unwrap());
    assert_ne!(t0, w.swap_recovery_tag(0, 1).unwrap());
    assert_ne!(t0, w.swap_recovery_tag(1, 0).unwrap());
    for chain in ["btc", "xbt"] {
        assert_ne!(t0, hex::encode(w.swap_secret(chain.into(), 0, 0).unwrap()));
    }
    let other = Wallet::from_mnemonic(BOB.into(), String::new(), "regtest".into()).unwrap();
    assert_ne!(t0, other.swap_recovery_tag(0, 0).unwrap());
}

/// The initiator's preimage takes the phone's own random nonce, so a swap
/// index used twice (after a restore whose counter an edge held back) still
/// never repeats a preimage, even in a match with the same id.
#[test]
fn the_initiator_preimage_takes_the_phones_nonce() {
    let alice = Phone::new(ALICE);
    let a1 = alice.session_with("initiator", NONCE).unwrap();
    let a2 = alice.session_with("initiator", "ffeeddccbbaa99887766554433221100").unwrap();
    assert_ne!(a1.hashlock(), a2.hashlock());
    assert_eq!(a1.preimage_nonce().as_deref(), Some(NONCE));
    // the same nonce (as relayed with the hashlock) rebuilds the same preimage
    let again = alice.session_with("initiator", NONCE).unwrap();
    assert_eq!(a1.hashlock(), again.hashlock());
    // and it survives the session's JSON
    let back = SwapSession::from_json(a1.to_json(), alice.xbt_secret.clone(), alice.btc_secret.clone()).unwrap();
    assert_eq!(a1.hashlock(), back.hashlock());
    assert_eq!(back.preimage_nonce().as_deref(), Some(NONCE));
}

#[test]
fn an_initiator_needs_a_16_byte_nonce() {
    let (alice, bob) = (Phone::new(ALICE), Phone::new(BOB));
    for bad in ["", "0102", "zz02030405060708090a0b0c0d0e0f10", "0102030405060708090a0b0c0d0e0f1011"] {
        assert!(alice.session_with("initiator", bad).is_err(), "{bad:?}");
    }
    let b = bob.session_with("participant", "").unwrap();
    assert_eq!(b.preimage_nonce(), None);
}

/// A restored initiator rebuilds its session with the nonce read back from the
/// relay; a wrong one (an edge lying) gives another preimage, which the hashlock
/// it relayed then refuses.
#[test]
fn a_wrong_relayed_nonce_is_refused_by_the_hashlock() {
    let (alice, bob) = (Phone::new(ALICE), Phone::new(BOB));
    let a = alice.session("initiator");
    let hashlock = a.hashlock().unwrap();
    let b = bob.session("participant");
    let restored = alice.session_with("initiator", NONCE).unwrap();
    restored.set_counterparty(b.our_pubkeys(), hashlock.clone()).unwrap();
    let lied = alice.session_with("initiator", "ffeeddccbbaa99887766554433221100").unwrap();
    let e = lied.set_counterparty(b.our_pubkeys(), hashlock).unwrap_err();
    assert!(e.to_string().contains("hashlock does not match our preimage"), "{e}");
}

/// Cancelled before the participant funded: it signs the initiator's XBT
/// back; the initiator checks that signature and finishes the release. No
/// preimage is revealed, and a bad signature is refused before broadcast.
#[test]
fn a_cancelled_swap_releases_the_initiators_coins() {
    let (_alice, _bob, a, b) = matched();
    let op = fake_txid(0x11);
    a.record_our_outpoint(op.clone(), 0).unwrap();
    let asks = a.release_txs(vec![2, 20]).unwrap();
    assert_eq!(asks.len(), 2);
    for ask in &asks {
        let sig = b.sign_release(ask.clone(), op.clone(), 0).unwrap();
        let hex = a.finish_release(ask.clone(), sig.clone()).unwrap();
        let tx: Transaction = deserialize_hex(&hex).unwrap();
        assert_eq!(tx.input[0].witness.len(), 5);
        assert_eq!(a.preimage_from_tx(hex.clone()), None, "a release reveals no preimage");
        // a signature by anyone else is refused
        let mut bad = hex::decode(&sig).unwrap();
        bad[10] ^= 1;
        assert!(a.finish_release(ask.clone(), hex::encode(bad)).is_err());
    }
    // the participant signs only a release of the initiator's contract
    assert!(b.sign_release(asks[0].clone(), fake_txid(0x12), 0).is_err());
}
