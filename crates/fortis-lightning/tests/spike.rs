//! Task 1 feasibility spike (findings recorded in
//! `docs/superpowers/specs/2026-10-30-lightning-design.md`, *Findings (Task 1)*).
//!
//! Both tests need real binaries and are `#[ignore]`d. Run with:
//!
//! ```sh
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test spike -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{builder, env_bin, poll, test_seed, wait_event, Regtest, TempDir, Vss};
use ldk_node::bitcoin::absolute::LockTime;
use ldk_node::bitcoin::bip32::{ChildNumber, Xpriv};
use ldk_node::bitcoin::consensus::encode::serialize_hex;
use ldk_node::bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use ldk_node::bitcoin::sighash::{EcdsaSighashType, SighashCache};
use ldk_node::bitcoin::transaction::Version;
use ldk_node::bitcoin::{
    ecdsa, Amount, CompressedPublicKey, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
};
use ldk_node::lightning::ln::chan_utils::get_to_countersigner_keyed_anchor_redeemscript;
use ldk_node::lightning::sign::{KeysManager, STATIC_PAYMENT_KEY_COUNT};
use ldk_node::{Event, Node};

const CHANNEL_SAT: u64 = 200_000;

fn sync(n: &Node) {
    n.sync_wallets().unwrap();
}

/// Two nodes, A (the "phone") opens a channel to B (the "LSP"). A then loses
/// all local state and is rebuilt from the same seed with an empty data dir;
/// B force-closes. The requirement: A ends up with the channel balance on
/// chain (swept by ldk-node into its on-chain wallet) after the timelock.
///
/// The restored A is created *before* the force-close confirms: the
/// bitcoind-RPC chain source does not rescan history (see `common`), and the
/// real flow is "restore, then ask the LSP to force-close" anyway.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND; see module docs"]
fn force_closed_channel_funds_recoverable_from_seed_only() {
    let Some(_) = env_bin("FORTIS_BTC_BITCOIND") else { return };
    let rt = Regtest::start();
    let dirs = TempDir::new("sweep");
    let (seed_a, seed_b) = (test_seed(0xa1), test_seed(0xb2));

    let (ba, _) = builder(&rt, seed_a, &dirs.str("a"));
    let a = ba.build().unwrap();
    a.start().unwrap();
    let (bb, b_addr) = builder(&rt, seed_b, &dirs.str("b"));
    let b = bb.build().unwrap();
    b.start().unwrap();

    // Fund both (B needs an on-chain reserve for anchor channels).
    rt.fund(&a.onchain_payment().new_address().unwrap().to_string(), 1_000_000);
    rt.fund(&b.onchain_payment().new_address().unwrap().to_string(), 500_000);
    rt.mine(6);
    poll("A and B funded", 60, || {
        sync(&a);
        sync(&b);
        (a.list_balances().spendable_onchain_balance_sats > 900_000
            && b.list_balances().spendable_onchain_balance_sats > 400_000)
            .then_some(())
    });

    a.open_channel(b.node_id(), b_addr.clone(), CHANNEL_SAT, None, None).unwrap();
    wait_event(&a, "A ChannelPending", 60, |e| matches!(e, Event::ChannelPending { .. }));
    rt.mine(6);
    wait_event(&a, "A ChannelReady", 60, |e| matches!(e, Event::ChannelReady { .. }));
    wait_event(&b, "B ChannelReady", 60, |e| matches!(e, Event::ChannelReady { .. }));
    let chan_b = b.list_channels().into_iter().next().expect("B's channel");
    let a_ln = a.list_balances().total_lightning_balance_sats;
    eprintln!("A lightning balance before loss: {a_ln} sat; A on-chain {}", a.list_balances().total_onchain_balance_sats);
    assert!(a_ln > 150_000);

    // Lose all of A's local state.
    a.stop().unwrap();
    drop(a);
    std::fs::remove_dir_all(dirs.path("a")).unwrap();

    // Restore A from seed alone, empty data dir.
    let (ba2, _) = builder(&rt, seed_a, &dirs.str("a2"));
    let a2 = ba2.build().unwrap();
    a2.start().unwrap();
    sync(&a2);
    let onchain_before = a2.list_balances().total_onchain_balance_sats;
    eprintln!("restored A: channels {:?}, on-chain {onchain_before} sat", a2.list_channels().len());
    assert!(a2.list_channels().is_empty(), "restored node must know no channel");

    // Restored A reconnects to B (LDK answers B's reestablish for an unknown
    // channel); B force-closes if it has not already.
    let _ = a2.connect(b.node_id(), b_addr, true);
    std::thread::sleep(Duration::from_secs(3));
    if b.list_channels().iter().any(|c| c.user_channel_id == chan_b.user_channel_id) {
        b.force_close_channel(&chan_b.user_channel_id, chan_b.counterparty_node_id, None).unwrap();
    }
    wait_event(&b, "B ChannelClosed", 60, |e| matches!(e, Event::ChannelClosed { .. }));

    // Find the commitment tx spending the funding output; mine it.
    let funding = chan_b.funding_txo.expect("funding txo");
    let commit_txid = poll("commitment tx in mempool", 60, || {
        sync(&b);
        let r = rt.json(&["gettxspendingprevout", &format!(r#"[{{"txid":"{}","vout":{}}}]"#, funding.txid, funding.vout)]);
        r[0]["spendingtxid"].as_str().map(str::to_string)
    });
    rt.mine(1);

    // Past the timelock (to_remote: 1 block CSV with anchors); give ldk-node
    // ample blocks and time to notice and sweep by itself.
    let mut native_swept = false;
    for _ in 0..5 {
        rt.mine(6);
        sync(&a2);
        let bal = a2.list_balances();
        eprintln!(
            "restored A: on-chain {} (spendable {}), pending from closures {:?}",
            bal.total_onchain_balance_sats, bal.spendable_onchain_balance_sats, bal.pending_balances_from_channel_closures
        );
        if bal.total_onchain_balance_sats > onchain_before + 150_000 {
            native_swept = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("FINDING (a): ldk-node 0.7.1 swept the output by itself from seed only: {native_swept}");

    // Seed-only recovery outside ldk-node: A's to_remote output pays one of
    // LDK 0.2's 2000 static, seed-derived scripts (`v2_remote_key_derivation`,
    // on in ldk-node 0.7.1). Find it, sign a spend with the seed-derived key,
    // pay the restored node's own on-chain address.
    let tx = rt.json(&["getrawtransaction", &commit_txid, "true"]);
    let static_spks = seed_only_counterparty_close_spks(&seed_a);
    let (vout, value, spk) = tx["vout"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|o| {
            let spk = ScriptBuf::from_hex(o["scriptPubKey"]["hex"].as_str().unwrap()).unwrap();
            let sat = (o["value"].as_f64().unwrap() * 1e8).round() as u64;
            static_spks.contains(&spk).then(|| (o["n"].as_u64().unwrap() as u32, sat, spk))
        })
        .expect("A's to_remote output pays a seed-derivable static script");
    eprintln!("commitment {commit_txid}:{vout} pays A {value} sat to a seed-derivable script");

    if !native_swept {
        let dest = a2.onchain_payment().new_address().unwrap();
        let prev = OutPoint { txid: commit_txid.parse().unwrap(), vout };
        let sweep = sweep_static_output(&seed_a, prev, value, &spk, &dest.script_pubkey(), 1_000)
            .expect("a seed-derived key for the script");
        let txid = rt.cli(&["sendrawtransaction", &serialize_hex(&sweep)]);
        eprintln!("seed-only sweep tx {txid} -> {dest}");
        rt.mine(6);
    }
    let bal = poll("restored A's on-chain wallet holds the channel balance", 60, || {
        sync(&a2);
        let b = a2.list_balances().total_onchain_balance_sats;
        (b > onchain_before + 150_000).then_some(b)
    });
    eprintln!("restored A on-chain balance: {bal} sat");
    let _ = (a2.stop(), b.stop());
}

/// The 2000 scripts LDK 0.2 pays our balance to when the counterparty
/// force-closes, derived the way ldk-node 0.7.1 builds its KeysManager (seed
/// = BIP32 master private key of the 64-byte entropy, v2 derivation on).
fn seed_only_counterparty_close_spks(seed: &[u8; 64]) -> Vec<ScriptBuf> {
    let km = KeysManager::new(&ldk_seed(seed), 0, 0, true);
    km.possible_v2_counterparty_closed_balance_spks(&Secp256k1::new())
}

/// ldk-node 0.7.1: KeysManager seed = master private key of the entropy.
fn ldk_seed(seed: &[u8; 64]) -> [u8; 32] {
    Xpriv::new_master(Network::Regtest, seed).unwrap().private_key.secret_bytes()
}

/// Spends a counterparty-force-close output paying our static payment key
/// (LDK 0.2: `KeysManager` master `/8'/idx'`, idx < 1000), from the seed
/// alone. Anchor channels: P2WSH(`<key> CHECKSIGVERIFY 1 CSV`); legacy
/// static_remote_key: P2WPKH.
fn sweep_static_output(
    seed: &[u8; 64], prev: OutPoint, value_sat: u64, spk: &ScriptBuf, to: &ScriptBuf, fee_sat: u64,
) -> Option<Transaction> {
    let secp = Secp256k1::new();
    let master = Xpriv::new_master(Network::Testnet, &ldk_seed(seed)).unwrap();
    let static_key = master.derive_priv(&secp, &[ChildNumber::from_hardened_idx(8).unwrap()]).unwrap();
    let value = Amount::from_sat(value_sat);
    for idx in 0..u32::from(STATIC_PAYMENT_KEY_COUNT) {
        let sk = static_key.derive_priv(&secp, &[ChildNumber::from_hardened_idx(idx).unwrap()]).unwrap().private_key;
        let pk = PublicKey::from_secret_key(&secp, &sk);
        let ws = get_to_countersigner_keyed_anchor_redeemscript(&pk);
        let anchor = ws.to_p2wsh() == *spk;
        let wpkh = ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash()) == *spk;
        if !anchor && !wpkh {
            continue;
        }
        let mut tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: if anchor { Sequence::from_height(1) } else { Sequence::ENABLE_RBF_NO_LOCKTIME },
                witness: Witness::new(),
            }],
            output: vec![TxOut { value: Amount::from_sat(value_sat - fee_sat), script_pubkey: to.clone() }],
        };
        let mut cache = SighashCache::new(&tx);
        let sighash = if anchor {
            cache.p2wsh_signature_hash(0, &ws, value, EcdsaSighashType::All).unwrap()
        } else {
            cache.p2wpkh_signature_hash(0, spk, value, EcdsaSighashType::All).unwrap()
        };
        let sig = ecdsa::Signature { signature: secp.sign_ecdsa(&Message::from(sighash), &sk), sighash_type: EcdsaSighashType::All };
        tx.input[0].witness = if anchor {
            Witness::from_slice(&[sig.to_vec(), ws.to_bytes()])
        } else {
            Witness::from_slice(&[sig.to_vec(), pk.serialize().to_vec()])
        };
        return Some(tx);
    }
    None
}

/// Two nodes from one seed against one VSS store: what does the second (or
/// the first, after the second wrote) get back? Records whether ldk-node
/// surfaces a distinct conflict error, then shows the alternative — a
/// `device-lock` key written with `vss-client` using VSS's conditional
/// versions.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER and PostgreSQL; see module docs"]
fn vss_conflict_is_observable() {
    let (Some(_), Some(_)) = (env_bin("FORTIS_BTC_BITCOIND"), env_bin("FORTIS_VSS_SERVER")) else { return };
    let rt = Regtest::start();
    let vss = Vss::start();
    let dirs = TempDir::new("vss-conflict");
    let seed = test_seed(0x55);
    let store_id = "fortis-spike-store".to_string();

    let build = |dir: &str| {
        let (b, _) = builder(&rt, seed, &dirs.str(dir));
        b.build_with_vss_store_and_fixed_headers(vss.url.clone(), store_id.clone(), HashMap::new())
    };

    let n1 = build("one").expect("first VSS node builds");
    n1.start().expect("first VSS node starts");
    let addr1 = n1.onchain_payment().new_address().expect("first node writes");
    rt.fund(&addr1.to_string(), 300_000);
    rt.mine(6);
    poll("first node funded", 60, || {
        sync(&n1);
        (n1.list_balances().spendable_onchain_balance_sats > 0).then_some(())
    });

    // Second device, same seed, same store, while the first is running.
    let n2_build = build("two");
    eprintln!("second node build: {:?}", n2_build.as_ref().map(|_| "Ok"));
    let n2 = n2_build.expect("second VSS node build");
    let n2_start = n2.start();
    eprintln!("second node start: {n2_start:?}");
    let n2_write = n2.onchain_payment().new_address();
    eprintln!("second node write (new_address): {n2_write:?}");
    let n2_bal = n2.list_balances().spendable_onchain_balance_sats;
    eprintln!("second node sees balance {n2_bal} sat (loaded first node's wallet state)");

    // The first device writes again after the second did.
    let n1_write = n1.onchain_payment().new_address();
    eprintln!("first node write after second wrote: {n1_write:?}");
    let n1_send = n1.onchain_payment().send_to_address(&addr1, 10_000, None);
    eprintln!("first node on-chain send after second wrote: {n1_send:?}");
    let n1_sync = n1.sync_wallets();
    eprintln!("first node sync after second wrote: {n1_sync:?}");

    let conflicts = vss.log().matches("possible conflict").count();
    eprintln!("vss-server conflicts reported while two nodes shared the store: {conflicts}");
    let _ = (n1.stop(), n2.stop());

    // FINDING (b): nothing above fails — ldk-node writes every key
    // unconditionally (version -1), so VSS never reports a conflict.
    assert!(n2_start.is_ok() && n2_write.is_ok() && n1_write.is_ok() && n1_sync.is_ok());
    assert_eq!(conflicts, 0, "vss-server reported a conflict: ldk-node uses conditional writes after all");

    // The alternative for Task 10: a device-lock key with conditional
    // versions, written via vss-client — the loser gets ConflictError.
    device_lock_conflict_via_vss_client(&vss.url, &store_id);
    assert_eq!(vss.log().matches("possible conflict").count(), 2, "vss-server logged both device-lock conflicts");
}

fn device_lock_conflict_via_vss_client(url: &str, store_id: &str) {
    use vss_client::client::VssClient;
    use vss_client::error::VssError;
    use vss_client::types::{GetObjectRequest, KeyValue, PutObjectRequest};
    use vss_client::util::retry::{ExponentialBackoffRetryPolicy, RetryPolicy};

    let rt = tokio_rt();
    let policy = ExponentialBackoffRetryPolicy::<VssError>::new(Duration::from_millis(10)).with_max_attempts(1);
    let client = VssClient::new(url.to_string(), policy);
    let put = |device: &str, version: i64| {
        let req = PutObjectRequest {
            store_id: store_id.to_string(),
            global_version: None,
            transaction_items: vec![KeyValue { key: "device-lock".into(), version, value: device.as_bytes().to_vec() }],
            delete_items: vec![],
        };
        rt.block_on(client.put_object(&req))
    };
    let get_version = || {
        let req = GetObjectRequest { store_id: store_id.to_string(), key: "device-lock".into() };
        rt.block_on(client.get_object(&req)).unwrap().value.unwrap().version
    };

    put("phone-1", 0).expect("phone 1 claims an absent lock (version 0 = insert-if-absent)");
    let v = get_version();
    let lost = put("phone-2", 0);
    eprintln!("phone 2 insert-if-absent over phone 1's lock: {lost:?}");
    assert!(matches!(lost, Err(VssError::ConflictError(_))), "{lost:?}");
    put("phone-2", v).expect("phone 2 takes over at the version it read");
    let stale = put("phone-1", v);
    eprintln!("phone 1 refreshing at its stale version: {stale:?}");
    assert!(matches!(stale, Err(VssError::ConflictError(_))), "{stale:?}");
}

fn tokio_rt() -> ldk_node::tokio::runtime::Runtime {
    ldk_node::tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}
