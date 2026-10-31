//! Seed-only recovery sweeper against real ldk-node channels (regtest).
//!
//! `#[ignore]`d (needs Bitcoin Core). Run with:
//!
//! ```sh
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//!   cargo test -p fortis-lightning --test recover -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::time::{Duration, Instant};

use common::{builder, env_bin, poll, test_seed, wait_event, Regtest, TempDir, RPC_PASS, RPC_USER};
use fortis_lightning::recover::{
    scan_static_outputs, static_scripts, sweep_static_outputs, BitcoindBroadcast, BitcoindScan,
    StaticUtxo,
};
use fortis_lightning::LnError;
use ldk_node::bitcoin::bip32::Xpriv;
use ldk_node::bitcoin::secp256k1::Secp256k1;
use ldk_node::bitcoin::{Address, Network, ScriptBuf, Txid};
use ldk_node::lightning::sign::KeysManager;
use ldk_node::{Event, Node};

const CHANNEL_SAT: u64 = 200_000;
const FEERATE: u64 = 2;

fn sync(n: &Node) {
    n.sync_wallets().unwrap();
}

fn scan(rt: &Regtest, seed: &[u8; 64]) -> Vec<StaticUtxo> {
    let src = BitcoindScan::new(&rt.rpc_url, RPC_USER, RPC_PASS);
    let mut calls = vec![];
    let t = Instant::now();
    let found =
        scan_static_outputs(seed, Network::Regtest, &src, &mut |d, n| calls.push((d, n))).unwrap();
    eprintln!(
        "scantxoutset over 2000 raw() descriptors: {:?}, {} found",
        t.elapsed(),
        found.len()
    );
    assert_eq!(calls, vec![(0, 2000), (2000, 2000)]);
    found
}

/// A opens a channel to B, loses all state and is restored from the same
/// seed; B force-closes. From the seed alone, the new API finds exactly A's
/// to_remote output on the commitment and sweeps it to an external address.
#[test]
#[ignore = "needs FORTIS_BTC_BITCOIND; see module docs"]
fn force_closed_balance_is_found_and_swept_from_seed_only() {
    let Some(_) = env_bin("FORTIS_BTC_BITCOIND") else {
        return;
    };
    let rt = Regtest::start();
    let dirs = TempDir::new("recover");
    let (seed_a, seed_b) = (test_seed(0xa7), test_seed(0xb8));

    // A seed that never had a channel: nothing to find.
    assert!(scan(&rt, &test_seed(0xc9)).is_empty());

    let (ba, _) = builder(&rt, seed_a, &dirs.str("a"));
    let a = ba.build().unwrap();
    a.start().unwrap();
    let (bb, b_addr) = builder(&rt, seed_b, &dirs.str("b"));
    let b = bb.build().unwrap();
    b.start().unwrap();
    rt.fund(
        &a.onchain_payment().new_address().unwrap().to_string(),
        1_000_000,
    );
    rt.fund(
        &b.onchain_payment().new_address().unwrap().to_string(),
        500_000,
    );
    rt.mine(6);
    poll("A and B funded", 60, || {
        sync(&a);
        sync(&b);
        (a.list_balances().spendable_onchain_balance_sats > 900_000
            && b.list_balances().spendable_onchain_balance_sats > 400_000)
            .then_some(())
    });
    a.open_channel(b.node_id(), b_addr.clone(), CHANNEL_SAT, None, None)
        .unwrap();
    wait_event(&a, "A ChannelPending", 60, |e| {
        matches!(e, Event::ChannelPending { .. })
    });
    rt.mine(6);
    wait_event(&a, "A ChannelReady", 60, |e| {
        matches!(e, Event::ChannelReady { .. })
    });
    wait_event(&b, "B ChannelReady", 60, |e| {
        matches!(e, Event::ChannelReady { .. })
    });
    let chan_b = b.list_channels().into_iter().next().expect("B's channel");
    // An open channel pays no static script yet.
    assert!(scan(&rt, &seed_a).is_empty());

    // Lose A's state; restore from seed with an empty data dir.
    a.stop().unwrap();
    drop(a);
    std::fs::remove_dir_all(dirs.path("a")).unwrap();
    let (ba2, _) = builder(&rt, seed_a, &dirs.str("a2"));
    let a2 = ba2.build().unwrap();
    a2.start().unwrap();
    sync(&a2);
    assert!(a2.list_channels().is_empty());
    let _ = a2.connect(b.node_id(), b_addr, true);
    std::thread::sleep(Duration::from_secs(3));
    if b.list_channels()
        .iter()
        .any(|c| c.user_channel_id == chan_b.user_channel_id)
    {
        b.force_close_channel(&chan_b.user_channel_id, chan_b.counterparty_node_id, None)
            .unwrap();
    }
    wait_event(&b, "B ChannelClosed", 60, |e| {
        matches!(e, Event::ChannelClosed { .. })
    });
    let funding = chan_b.funding_txo.expect("funding txo");
    let commit_txid = poll("commitment tx in mempool", 60, || {
        sync(&b);
        let r = rt.json(&[
            "gettxspendingprevout",
            &format!(r#"[{{"txid":"{}","vout":{}}}]"#, funding.txid, funding.vout),
        ]);
        r[0]["spendingtxid"].as_str().map(str::to_string)
    });
    rt.mine(1);

    // Scan from the seed alone: exactly one output, on the commitment.
    let found = scan(&rt, &seed_a);
    assert_eq!(found.len(), 1, "{found:?}");
    let u = found[0].clone();
    assert_eq!(u.outpoint.txid.to_string(), commit_txid);
    assert_eq!(u.confirmations, 1);
    let commit = rt.json(&["getrawtransaction", &commit_txid, "true"]);
    let out = &commit["vout"][u.outpoint.vout as usize];
    let spk = ScriptBuf::from_hex(out["scriptPubKey"]["hex"].as_str().unwrap()).unwrap();
    assert_eq!(
        (out["value"].as_f64().unwrap() * 1e8).round() as u64,
        u.value_sat
    );
    assert!(
        u.value_sat > 190_000 && u.value_sat < CHANNEL_SAT,
        "A's balance minus commitment fee/anchors: {}",
        u.value_sat
    );
    eprintln!(
        "to_remote {}: {} sat, static script index {}",
        u.outpoint, u.value_sat, u.script_index
    );
    // The script is the one ldk-node's own KeysManager lists, and an anchor P2WSH.
    assert!(spk.is_p2wsh() && u.script_index % 2 == 1);
    assert_eq!(
        static_scripts(&seed_a, Network::Regtest)[u.script_index as usize].1,
        spk
    );
    let master = Xpriv::new_master(Network::Regtest, &seed_a).unwrap();
    let km = KeysManager::new(&master.private_key.secret_bytes(), 1, 2, true);
    assert!(km
        .possible_v2_counterparty_closed_balance_spks(&Secp256k1::new())
        .contains(&spk));

    // Sweep to an external (miner wallet) address.
    let to: Address = rt
        .cli(&["-rpcwallet=miner", "getnewaddress", "sweep", "bech32"])
        .parse::<Address<_>>()
        .unwrap()
        .require_network(Network::Regtest)
        .unwrap();
    let bc = BitcoindBroadcast::new(&rt.rpc_url, RPC_USER, RPC_PASS);
    let txid: Txid = sweep_static_outputs(&seed_a, Network::Regtest, &found, &to, FEERATE, &bc)
        .unwrap()
        .expect("swept");
    rt.mine(1);
    let sweep = rt.json(&["getrawtransaction", &txid.to_string(), "true"]);
    assert!(
        sweep["confirmations"].as_u64().unwrap() >= 1,
        "sweep confirmed"
    );
    let vsize = sweep["vsize"].as_u64().unwrap();
    let outs = sweep["vout"].as_array().unwrap();
    assert_eq!(outs.len(), 1);
    assert_eq!(
        outs[0]["scriptPubKey"]["address"].as_str().unwrap(),
        to.to_string()
    );
    let received = (outs[0]["value"].as_f64().unwrap() * 1e8).round() as u64;
    assert_eq!(
        received,
        u.value_sat - vsize * FEERATE,
        "received = output value - vsize × feerate"
    );
    let spent = rt.cli(&["gettxout", &commit_txid, &u.outpoint.vout.to_string()]);
    assert!(spent.is_empty(), "to_remote output is spent: {spent}");
    let got: f64 = rt
        .cli(&[
            "-rpcwallet=miner",
            "getreceivedbyaddress",
            &to.to_string(),
            "1",
        ])
        .parse()
        .unwrap();
    assert_eq!((got * 1e8).round() as u64, received);
    eprintln!("sweep {txid}: {received} sat to {to} ({vsize} vB at {FEERATE} sat/vB)");

    // Nothing left: a second scan finds nothing, a sweep of it is a no-op,
    // and re-sweeping the stale scan result is a surfaced broadcast failure.
    let again = scan(&rt, &seed_a);
    assert!(again.is_empty());
    assert_eq!(
        sweep_static_outputs(&seed_a, Network::Regtest, &again, &to, FEERATE, &bc),
        Ok(None)
    );
    let stale = sweep_static_outputs(&seed_a, Network::Regtest, &found, &to, FEERATE + 1, &bc);
    eprintln!("re-sweep of a spent output: {stale:?}");
    assert!(
        matches!(&stale, Err(LnError::Other(m)) if m.starts_with("broadcast_failed:")),
        "{stale:?}"
    );

    let _ = (a2.stop(), b.stop());
}
