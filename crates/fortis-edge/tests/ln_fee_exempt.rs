//! The wallet's Lightning node broadcasts through `POST /<chain>/tx` and cannot add the
//! service-fee output, so its own transactions (channel opens, closes, sweeps) must pass
//! the edge's fee check — on BOTH chains. Ordinary sends without the fee stay refused.
//!
//! Why: on 2026-11-16 the exemption applied to BTC only. The phone's XBT channel funding
//! transaction was refused ("402 transaction must pay the service fee") and the channel
//! stayed pending for ever; XBT closes and force closes would have been refused too.
//!
//! A real `fortis-edge` process over HTTP; the chain upstream is a stub that serves parent
//! transactions and records what was broadcast. No bitcoind.

mod common;

use std::sync::{Arc, Mutex};

use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::{absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use common::{agent, bin, free_port, req, spawn, wait_up, TempDir};
use lightning::ln::chan_utils::make_funding_redeemscript;
use serde_json::Value;

/// A mainnet P2WPKH address: the service-fee address.
const FEE_ADDRESS: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

fn hex(t: &Transaction) -> String {
    ::hex::encode(bitcoin::consensus::serialize(t))
}

fn pk(n: u8) -> PublicKey {
    PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}

fn spend(prev: Txid, witness: Vec<Vec<u8>>) -> TxIn {
    TxIn {
        previous_output: OutPoint { txid: prev, vout: 0 },
        script_sig: ScriptBuf::new(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&witness),
    }
}

fn tx(inputs: Vec<TxIn>, outputs: Vec<(u64, ScriptBuf)>) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: inputs,
        output: outputs
            .into_iter()
            .map(|(sat, script_pubkey)| TxOut { value: Amount::from_sat(sat), script_pubkey })
            .collect(),
    }
}

/// A chain upstream: `GET /tx/<id>/hex` for the known transactions, `POST /tx` records the
/// body and answers its txid.
struct Upstream {
    url: String,
    broadcast: Arc<Mutex<Vec<String>>>,
}

fn upstream(known: Vec<Transaction>) -> Upstream {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let broadcast = Arc::new(Mutex::new(Vec::new()));
    let seen = broadcast.clone();
    std::thread::spawn(move || {
        for mut r in server.incoming_requests() {
            let path = r.url().to_string();
            if r.method() == &tiny_http::Method::Post && path == "/tx" {
                let mut body = String::new();
                std::io::Read::read_to_string(r.as_reader(), &mut body).unwrap();
                let id = bitcoin::consensus::deserialize::<Transaction>(&::hex::decode(body.trim()).unwrap())
                    .unwrap()
                    .compute_txid()
                    .to_string();
                seen.lock().unwrap().push(body);
                let _ = r.respond(tiny_http::Response::from_string(id));
                continue;
            }
            let hit = known.iter().find(|t| path == format!("/tx/{}/hex", t.compute_txid()));
            let _ = match hit {
                Some(t) => r.respond(tiny_http::Response::from_string(hex(t))),
                None => r.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
            };
        }
    });
    Upstream { url, broadcast }
}

fn fee_script() -> ScriptBuf {
    use std::str::FromStr;
    bitcoin::Address::from_str(FEE_ADDRESS).unwrap().assume_checked().script_pubkey()
}

#[test]
fn lightning_transactions_pass_the_fee_check_on_btc_and_xbt_and_ordinary_sends_do_not() {
    let wallet = vec![vec![0x30; 72], vec![0x02; 33]];
    let genesis = Txid::from_byte_array([1; 32]);
    // "Move to Lightning": a deposit that paid the fee (1 % output), then the node's channel
    // open spending it with no fee output of its own.
    let deposit = tx(
        vec![spend(genesis, wallet.clone())],
        vec![(144_590, ScriptBuf::new()), (712, fee_script()), (4_354, ScriptBuf::new())],
    );
    let open = tx(vec![spend(deposit.compute_txid(), wallet.clone())], vec![(144_190, ScriptBuf::new())]);
    // A cooperative / force close spends the channel's 2-of-2 funding output.
    let funding = make_funding_redeemscript(&pk(2), &pk(3)).into_bytes();
    let close = tx(
        vec![spend(open.compute_txid(), vec![vec![], vec![0x30; 72], vec![0x30; 72], funding])],
        vec![(100_000, ScriptBuf::new())],
    );
    // An ordinary send of coins that never paid the fee.
    let plain_parent = tx(vec![spend(genesis, wallet.clone())], vec![(200_000, ScriptBuf::new())]);
    let plain_send = tx(vec![spend(plain_parent.compute_txid(), wallet.clone())], vec![(150_000, ScriptBuf::new())]);
    // A send that pays the fee itself.
    let paying = tx(
        vec![spend(plain_parent.compute_txid(), wallet)],
        vec![(150_000, ScriptBuf::new()), (800, fee_script())],
    );

    for chain in ["btc", "xbt"] {
        let up = upstream(vec![deposit.clone(), plain_parent.clone()]);
        let dir = TempDir::new(&format!("ln-fee-{chain}"));
        let port = free_port();
        let p = |n: &str| dir.path(n).to_string_lossy().into_owned();
        let args: Vec<String> = [
            "--bind", &format!("127.0.0.1:{port}"),
            "--xbt-upstream", &up.url,
            "--btc-upstream", &up.url,
            "--btc-upstream-fallback=",
            "--secret-file", &p("edge.secret"),
            "--cache-dir", &p("edge-cache"),
            "--btc-history-dir", &p("edge-btc-history"),
            "--btc-scan-cache-dir", &p("edge-btc-scan-cache"),
            "--register-per-hour", "1000",
            "--workers", "8",
            "--service-fee-address", FEE_ADDRESS,
            "--service-fee-floor-sat", "400",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let _svc = spawn(&bin("fortis-edge"), &argv, Some(dir.path("edge.log")));
        let base = format!("http://127.0.0.1:{port}");
        wait_up(&agent(), &format!("{base}/"));
        let a = agent();
        let (s, body) = req(&a, "POST", &format!("{base}/register"), &[], Some(""));
        assert_eq!(s, 200, "{chain} register: {body}");
        let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
        let auth = format!("Bearer {token}");
        let post = |t: &Transaction| req(&a, "POST", &format!("{base}/{chain}/tx"), &[("Authorization", &auth)], Some(&hex(t)));

        let (s, b) = post(&open);
        assert_eq!(s, 200, "{chain}: the channel open must pass: {b}");
        let (s, b) = post(&close);
        assert_eq!(s, 200, "{chain}: a close must pass: {b}");
        let (s, b) = post(&paying);
        assert_eq!(s, 200, "{chain}: a send paying the fee passes: {b}");
        let before = up.broadcast.lock().unwrap().len();
        let (s, b) = post(&plain_send);
        assert_eq!(s, 402, "{chain}: an ordinary fee-less send is refused: {b}");
        assert_eq!(up.broadcast.lock().unwrap().len(), before, "{chain}: the refused tx must not reach the upstream");
        assert_eq!(up.broadcast.lock().unwrap().len(), 3, "{chain}: open, close and the paying send were broadcast");
    }
}
