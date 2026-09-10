//! fortis-index — an address index over a Bitcoin Knots / BLAKE2b node, served
//! through the same Esplora REST subset the fortis wallet's `EsploraBackend`
//! already speaks. Unlike the `fortisd` gateway it keeps **no per-user state**:
//! the client derives its own addresses and scans them, so one instance serves
//! any number of wallets.
//!
//! It indexes from `--start-height` forward (the BLAKE2b fork height by default),
//! which covers every wallet created in the app. Pre-fork coins are out of scope
//! for now — a restored old seed's historical UTXOs won't appear.

mod api;
mod mempool;
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

/// BLAKE2b fork activation height on mainnet — the natural place to start
/// indexing (every in-app wallet's activity is at or after it).
const XBT_FORK_HEIGHT: u64 = 961_640;

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
    /// First block to index. Default: the fork height on mainnet, 0 otherwise.
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
    let start_height = args.start_height.unwrap_or(if regtest { 0 } else { XBT_FORK_HEIGHT });

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
