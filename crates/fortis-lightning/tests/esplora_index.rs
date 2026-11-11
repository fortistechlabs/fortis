//! The client `Node` with an **Esplora** chain source pointed at the real
//! `fortis-index` — what phones use (through the edge), and what every other
//! real-stack test bypasses by giving the client bitcoind RPC. Covers each
//! chain-facing step of a user-funded channel's life: fee estimates at start,
//! finding a deposit by script hash, broadcasting the channel open, seeing it
//! confirm (headers, merkle proofs, block status), the cooperative close
//! (outspends) and sweeping the closed balance back out.
//!
//! ```sh
//! cargo build -p fortis-lsp -p fortis-index
//! FORTIS_BTC_BITCOIND=/opt/bitcoin-core/current/bin/bitcoind \
//! FORTIS_VSS_SERVER=$(crates/fortis-lightning/scripts/build-vss-server.sh) \
//!   cargo test -p fortis-lightning --test esplora_index -- --ignored --nocapture --test-threads=1
//! ```

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{free_port, poll, test_seed, Lsp, Regtest, TempDir, Vss, RPC_PASS, RPC_USER};
use fortis_lightning::{ChainSource, ChannelState, LnEvent, Node, NodeConfig, VssConfig};
use ldk_node::bitcoin::Network;

/// The `fortis-index` binary next to this test binary (`target/<profile>/`).
fn index_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().and_then(|d| d.parent()).unwrap().join("fortis-index");
    assert!(p.exists(), "{} not built — run `cargo build -p fortis-index` first", p.display());
    p
}

/// A running `fortis-index` over the regtest node; killed on drop.
struct Index {
    child: Child,
    url: String,
    _dir: TempDir,
}

impl Drop for Index {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Index {
    fn start(rt: &Regtest) -> Index {
        let dir = TempDir::new("fortis-index");
        let port = free_port();
        let mut child = Command::new(index_bin())
            .args(["--network", "regtest", "--start-height", "0"])
            .args(["--rpc-url", &format!("http://127.0.0.1:{}", rt.rpc_port)])
            .args(["--rpc-auth", &format!("{RPC_USER}:{RPC_PASS}")])
            .arg("--db")
            .arg(dir.path("db"))
            .args(["--bind", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn fortis-index");
        let stderr = child.stderr.take().unwrap();
        let log_path = dir.path("index.log");
        LOG.with(|l| *l.borrow_mut() = log_path.clone());
        std::thread::spawn(move || {
            let mut f = std::fs::File::create(&log_path).unwrap();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[fortis-index] {line}");
                let _ = std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes());
            }
        });
        let idx = Index { child, url: format!("http://127.0.0.1:{port}"), _dir: dir };
        idx.wait_for_height(rt.height());
        idx
    }

    fn get(&self, path: &str) -> Option<String> {
        ureq::get(&format!("{}{path}", self.url))
            .timeout(Duration::from_secs(5))
            .call()
            .ok()?
            .into_string()
            .ok()
    }

    /// Until the index's tip reaches `h`.
    fn wait_for_height(&self, h: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if self.get("/blocks/tip/height").and_then(|s| s.trim().parse::<u64>().ok()) >= Some(h) {
                return;
            }
            assert!(Instant::now() < deadline, "fortis-index not at height {h}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn wait_event(client: &Node, what: &str, secs: u64, pred: impl Fn(&LnEvent) -> bool) -> LnEvent {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        if let Some(e) = client.next_event(left.min(Duration::from_secs(1))) {
            if pred(&e) {
                return e;
            }
            eprintln!("({what}: skipping {e:?})");
        }
    }
}

#[test]
#[ignore = "needs fortis-lsp and fortis-index built, FORTIS_BTC_BITCOIND, FORTIS_VSS_SERVER, initdb/pg_ctl"]
fn a_channel_lives_and_closes_over_the_esplora_index() {
    const DEPOSIT_SAT: u64 = 400_000;
    const CHANNEL_SAT: u64 = 200_000;
    let rt = Regtest::start();
    let vss = Vss::start();
    let lsp = Lsp::start(&rt, &[]);
    let index = Index::start(&rt);
    let dirs = TempDir::new("esplora-client");
    let client = Node::new(NodeConfig {
        chain: fortis_lightning::LnChain::Btc,
        bip39_seed: test_seed(0x3E).to_vec(),
        network: Network::Regtest,
        data_dir: dirs.path("client"),
        chain_source: ChainSource::Esplora { url: index.url.clone(), auth_header: None },
        lsp: lsp.lsp_info(),
        vss: VssConfig { url: vss.url.clone(), auth_header: "Bearer test".into() },
        limits: lsp.limits(),
    })
    .unwrap();
    // Start fetches fee estimates and syncs both wallets over the index.
    client.start().expect("client starts on the Esplora index");

    // A deposit to the transit wallet, found by script hash.
    let transit = client.transit_address().unwrap();
    rt.fund(&transit, DEPOSIT_SAT);
    rt.mine(6);
    index.wait_for_height(rt.height());
    poll("the deposit is seen through the index", 240, || {
        (client.balances().onchain_transit_sat >= DEPOSIT_SAT).then_some(())
    });

    // The open: the client broadcasts the funding through the index, then
    // follows its confirmations (headers, merkle proof, block status).
    client.open_channel(CHANNEL_SAT).expect("open");
    poll("funding in the mempool", 60, || {
        let n = rt.json(&["getmempoolinfo"])["size"].as_u64()?;
        (n > 0).then_some(())
    });
    rt.mine(6);
    index.wait_for_height(rt.height());
    wait_event(&client, "ChannelReady", 240, |e| matches!(e, LnEvent::ChannelReady));
    assert_eq!(client.channel_state(), ChannelState::Ready);
    poll("the channel's balance shows", 120, || {
        (client.balances().lightning_sat > 0).then_some(())
    });

    // The cooperative close, seen spent through the index's outspends, its
    // output found by script hash.
    client.close_channel().expect("cooperative close");
    poll("close in the mempool", 120, || {
        let n = rt.json(&["getmempoolinfo"])["size"].as_u64()?;
        (n > 0).then_some(())
    });
    rt.mine(6);
    index.wait_for_height(rt.height());
    poll("closed, and its balance back in the transit wallet", 300, || {
        let b = client.balances();
        (client.channel_state() == ChannelState::None && b.pending_close_sat == 0 && b.onchain_transit_sat > 0)
            .then_some(())
    });

    // Sweep it out: broadcast through the index, confirmed.
    let dest = rt.cli(&["-rpcwallet=miner", "getnewaddress"]);
    let txid = client.sweep_transit(&dest).expect("sweep").expect("something to sweep");
    let deadline = Instant::now() + Duration::from_secs(90);
    while rt.try_cli(&["getmempoolentry", &txid]).is_err() {
        if Instant::now() > deadline {
            let log = std::fs::read_to_string(dirs.path("client").join("ldk_node.log")).unwrap_or_default();
            let tail: Vec<&str> = log
                .lines()
                .filter(|l| !l.contains(" TRACE ") || l.to_lowercase().contains("broadcast"))
                .filter(|l| !l.contains("peer_handler"))
                .collect();
            let tail = tail[tail.len().saturating_sub(60)..].join("\n");
            panic!(
                "sweep {txid} never reached the mempool; its inputs:\n{}\ntransactions that rebuild wrong:\n{}\nclient log:\n{tail}",
                explain_inputs(&rt, &index, &txid),
                misrebuilt(&rt, &index)
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    rt.mine(1);
    let got = rt.json(&["-rpcwallet=miner", "getreceivedbyaddress", &dest, "1"]);
    assert!(got.as_f64().unwrap() > 0.0, "swept to {dest}: {got}");
    client.stop();
}

/// Every transaction in the regtest chain whose `/tx/:txid` JSON from the
/// index does not rebuild (esplora-client's `to_tx()`, what ldk-node's wallet
/// stores) to its own txid, with that JSON.
fn misrebuilt(rt: &Regtest, index: &Index) -> String {
    let mut bad = Vec::new();
    for h in 1..=rt.height() {
        let hash = rt.cli(&["getblockhash", &h.to_string()]);
        let block = rt.json(&["getblock", hash.trim(), "1"]);
        for id in block["tx"].as_array().unwrap() {
            let id = id.as_str().unwrap();
            let Some(body) = index.get(&format!("/tx/{id}")) else {
                bad.push(format!("{id}: not served"));
                continue;
            };
            match serde_json::from_str::<esplora_client::api::Tx>(&body) {
                Ok(t) if t.to_tx().compute_txid().to_string() == id => {}
                Ok(t) => bad.push(format!("{id} rebuilds as {}: {body}", t.to_tx().compute_txid())),
                Err(e) => bad.push(format!("{id}: unparseable ({e}): {body}")),
            }
        }
    }
    bad.join("\n")
}

/// For each input of the wallet's transaction `txid` (from ldk-node's own
/// broadcast, read back from the index's rejection log is not possible, so
/// from the wallet): which chain transaction, by txid or wtxid, it names, and
/// what the index answers for it.
fn explain_inputs(rt: &Regtest, index: &Index, txid: &str) -> String {
    let _ = txid;
    // The rejected sweep never reached the node; its inputs are in the
    // index's log line. Map every chain tx's wtxid and txid for lookup.
    let mut by_id = std::collections::HashMap::new();
    for h in 1..=rt.height() {
        let hash = rt.cli(&["getblockhash", &h.to_string()]);
        let block = rt.json(&["getblock", hash.trim(), "2"]);
        for t in block["tx"].as_array().unwrap() {
            let id = t["txid"].as_str().unwrap().to_string();
            by_id.insert(t["hash"].as_str().unwrap().to_string(), format!("wtxid of {id} (height {h})"));
            by_id.insert(id.clone(), format!("txid {id} (height {h})"));
        }
    }
    let ids: Vec<String> = std::fs::read_to_string(LOG.with(|l| l.borrow().clone()))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("broadcast rejected"))
        .flat_map(|l| l.split("spends ").nth(1).unwrap_or("").trim_end_matches(')').split(", ").map(|s| s.to_string()).collect::<Vec<_>>())
        .collect();
    ids.iter()
        .map(|op| {
            let t = op.split(':').next().unwrap_or("");
            let vout = op.split(':').nth(1).unwrap_or("0");
            let tx: serde_json::Value = index
                .get(&format!("/tx/{t}"))
                .and_then(|b| serde_json::from_str(&b).ok())
                .unwrap_or_default();
            let spk = tx["vout"][vout.parse::<usize>().unwrap_or(0)]["scriptpubkey"].as_str().unwrap_or("").to_string();
            let sh = {
                use ldk_node::bitcoin::hashes::{sha256, Hash};
                format!("{:x}", sha256::Hash::hash(&hex::decode(&spk).unwrap_or_default()))
            };
            let outspend = ureq::get(&format!("{}/tx/{t}/outspend/{vout}", index.url))
                .call()
                .map(|r| r.into_string().unwrap_or_default())
                .unwrap_or_else(|e| format!("error: {e}"));
            let hist: serde_json::Value = index
                .get(&format!("/scripthash/{sh}/txs"))
                .and_then(|b| serde_json::from_str(&b).ok())
                .unwrap_or_default();
            let hist_ids: Vec<String> = hist
                .as_array()
                .map(|a| a.iter().map(|t| format!("{} ({})", t["txid"], t["status"]["confirmed"])).collect())
                .unwrap_or_default();
            format!(
                "{op}: {}\n  node gettxout: {}\n  index outspend: {outspend}\n  index history of its script: {hist_ids:?}",
                by_id.get(t).cloned().unwrap_or_else(|| "on no chain tx".into()),
                rt.try_cli(&["gettxout", t, vout]).unwrap_or_default().trim(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

thread_local! {
    /// Where the index's stderr is copied (for `explain_inputs`).
    static LOG: std::cell::RefCell<std::path::PathBuf> = std::cell::RefCell::new(std::path::PathBuf::new());
}
