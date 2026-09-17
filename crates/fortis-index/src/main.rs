//! fortis-index — an address index over a Bitcoin Knots / BLAKE2b node, served
//! through the same Esplora REST subset the fortis wallet's `EsploraBackend`
//! already speaks. Unlike the `fortisd` gateway it keeps **no per-user state**:
//! the client derives its own addresses and scans them, so one instance serves
//! any number of wallets.
//!
//! It indexes from `--start-height` forward — SegWit activation (block 481824)
//! by default, so a wallet's pre-fork Bitcoin history (inherited by the XBT
//! chain at the hard fork) is included, not just activity since the fork.
//! Genesis-to-SegWit blocks are skipped, not just as an optimization: this
//! wallet derives exclusively BIP-84 P2WPKH addresses, and P2WPKH didn't
//! exist before SegWit, so no fortis wallet address can have history in that
//! range — indexing it can never find anything. (`block_txs` in `sync.rs`
//! also only stores P2WPKH outputs for the same reason, for every height.)
//! Pass `--start-height 961640` (the BLAKE2b fork height) to skip pre-fork
//! blocks entirely and index even faster when pre-fork coins don't matter
//! (e.g. regtest, or a wallet known to postdate the fork).

mod api;
mod mempool;
mod spend_filter;
mod store;
mod sync;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use fortis_node::Rpc;
use mempool::Mempool;
use store::Store;
use sync::Syncer;

/// Mainnet SegWit (BIP141) activation — the earliest block that can contain a
/// P2WPKH output, which is the only address type any fortis wallet ever
/// derives (`Address::p2wpkh`, `wallet-core/src/wallet.rs`). Nothing before
/// this height can possibly be a fortis wallet's history.
const SEGWIT_ACTIVATION_HEIGHT: u64 = 481_824;

#[derive(Parser)]
#[command(name = "fortis-index", version, about = "address index → Esplora REST (holds no keys)")]
struct Args {
    /// Node RPC URL. Default: 127.0.0.1:8332 (:18443 for regtest).
    #[arg(long)]
    rpc_url: Option<String>,
    /// Node data directory (for its `.cookie`). Default: the platform Bitcoin dir.
    #[arg(long)]
    datadir: Option<PathBuf>,
    /// Explicit RPC cookie file (overrides `--datadir`).
    #[arg(long)]
    cookie_file: Option<String>,
    /// Static RPC credentials as `user:password` (a bitcoin.conf `rpcauth` entry) —
    /// survives node restarts, unlike a cookie file. Overrides `--cookie-file` /
    /// `--datadir`.
    #[arg(long, value_name = "USER:PASS")]
    rpc_auth: Option<String>,
    /// Node network: mainnet | regtest.
    #[arg(long, default_value = "mainnet")]
    network: String,
    /// SQLite index file.
    #[arg(long, default_value = "fortis-index.sqlite")]
    db: String,
    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8094")]
    bind: String,
    /// First block to index. Default: 481824 (SegWit activation) — the
    /// earliest height any fortis wallet address (P2WPKH-only) could
    /// possibly have history, so pre-fork Bitcoin history is still fully
    /// covered without wasting time on blocks that provably can't contain
    /// any. Pass 961640 (the BLAKE2b fork height) to skip pre-fork blocks
    /// entirely and index even faster when pre-fork coins don't matter.
    #[arg(long)]
    start_height: Option<u64>,
    /// Seconds between catch-up passes.
    #[arg(long, default_value_t = 10)]
    poll: u64,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let regtest = args.network.starts_with("regtest");

    let rpc_url = args
        .rpc_url
        .clone()
        .unwrap_or_else(|| if regtest { "http://127.0.0.1:18443".into() } else { "http://127.0.0.1:8332".into() });
    let rpc = match (&args.rpc_auth, &args.cookie_file) {
        (Some(auth), _) => Rpc::new(&rpc_url, auth.trim()),
        (None, Some(cf)) => {
            Rpc::new(&rpc_url, std::fs::read_to_string(cf).with_context(|| format!("reading {cf}"))?.trim())
        }
        (None, None) => {
            let dd = args
                .datadir
                .clone()
                .unwrap_or_else(default_bitcoin_datadir)
                .to_string_lossy()
                .into_owned();
            Rpc::new_cookie(&rpc_url, &dd, &args.network)?
        }
    };
    let rpc = Arc::new(rpc);

    let cs = fortis_node::chain_status(&rpc).context("reaching the node")?;
    let network = if regtest { bitcoin::Network::Regtest } else { bitcoin::Network::Bitcoin };
    // Regtest has its own genesis and activates SegWit from block 0 (chain
    // params, not a real mainnet-style activation height), so mainnet's
    // SegWit-activation floor doesn't apply there — keep 0.
    let start_height = args.start_height.unwrap_or(if regtest { 0 } else { SEGWIT_ACTIVATION_HEIGHT });

    eprintln!("fortis-index → {rpc_url}  ({}, chain {})", cs.subversion, cs.chain);
    eprintln!("             db {}  ·  indexing from height {start_height}", args.db);

    // Ensure the file + schema exist before the reader opens it.
    let writer = Store::open(&args.db)?;
    let mempool = Arc::new(RwLock::new(Mempool::default()));

    {
        let rpc = Arc::clone(&rpc);
        let mempool = Arc::clone(&mempool);
        let poll = args.poll.max(1);
        let mut store = writer;
        thread::Builder::new()
            .name("indexer".into())
            .spawn(move || indexer_loop(&rpc, &mut store, &mempool, start_height, poll))
            .context("spawning indexer thread")?;
    }

    let reader = Store::open_readonly(&args.db)?;
    api::serve(&args.bind, reader, rpc, mempool, network)
}

fn indexer_loop(
    rpc: &Rpc,
    store: &mut Store,
    mempool: &RwLock<Mempool>,
    start_height: u64,
    poll: u64,
) {
    let mut local = Mempool::default();
    loop {
        match (Syncer { rpc, store, start_height }).sync_to_tip() {
            Ok((tip, applied)) if applied > 0 => eprintln!("index: +{applied} block(s), tip {tip}"),
            Ok(_) => {}
            Err(e) => eprintln!("index: {e:#}"),
        }
        match local.refresh(rpc) {
            Ok(()) => *mempool.write().unwrap() = local.clone(),
            Err(e) => eprintln!("mempool: {e:#}"),
        }
        thread::sleep(Duration::from_secs(poll));
    }
}

fn default_bitcoin_datadir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("Bitcoin")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".bitcoin")
    }
}
