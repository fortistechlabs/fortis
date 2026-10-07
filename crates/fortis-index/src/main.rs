//! fortis-index — a P2WPKH address index over a Bitcoin Core node (BTC) or a
//! Bitcoin Knots / BLAKE2b node (XBT), served through the Esplora REST subset
//! the fortis wallet's `EsploraBackend` speaks. It keeps **no per-user
//! state**: the client derives its own addresses and scans them, so one
//! instance serves any number of wallets.
//!
//! It indexes from `--start-height` forward — SegWit activation (481 824) by
//! default: this wallet derives only BIP-84 P2WPKH addresses, which cannot
//! appear before SegWit, so earlier blocks provably hold no wallet history.
//! Design: `docs/superpowers/specs/2026-10-07-fortis-index-v2-design.md`.

mod api;
mod chain;
mod chainstate;
mod db;
mod extract;
mod fetch;
mod keys;
mod mempool;
mod render;
mod source;
mod sync;
mod verify;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;

use fortis_node::Rpc;

use crate::chain::HeaderFormat;
use crate::chainstate::{BlockTable, SharedChain};
use crate::db::{Db, DbConfig};
use crate::fetch::FetchConfig;
use crate::mempool::{MempoolTracker, MempoolView, SharedMempool};
use crate::render::{Renderer, TxSource};
use crate::source::{BlockSource, RpcSource};
use crate::sync::{is_fatal, Backoff, SyncStatus, Syncer};

/// Mainnet SegWit (BIP141) activation — the earliest block that can contain a
/// P2WPKH output, the only address type any fortis wallet derives.
const SEGWIT_ACTIVATION_HEIGHT: u32 = 481_824;

/// Exit code for errors a restart cannot fix (systemd: RestartPreventExitStatus=2).
const EXIT_FATAL: u8 = 2;

#[derive(Parser)]
#[command(
    name = "fortis-index",
    version,
    about = "address index → Esplora REST (holds no keys)"
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Node RPC URL. Default: 127.0.0.1:8332 (:18443 for regtest).
    #[arg(long, global = true)]
    rpc_url: Option<String>,
    /// Node data directory (for its `.cookie`). Default: the platform Bitcoin dir.
    #[arg(long, global = true)]
    datadir: Option<PathBuf>,
    /// Explicit RPC cookie file (overrides `--datadir`). Re-read when the node
    /// restarts and rotates it.
    #[arg(long, global = true)]
    cookie_file: Option<PathBuf>,
    /// Static RPC credentials as `user:password`. Overrides `--cookie-file` / `--datadir`.
    #[arg(long, value_name = "USER:PASS", global = true)]
    rpc_auth: Option<String>,
    /// Node network: mainnet | regtest.
    #[arg(long, default_value = "mainnet", global = true)]
    network: String,
    /// RocksDB index directory (created if missing). Must not hold a v1 index.
    #[arg(long, default_value = "fortis-index-rocksdb", global = true)]
    db: PathBuf,
    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8094")]
    bind: String,
    /// First block to index. Default: 481824 (SegWit activation); 0 on regtest.
    #[arg(long)]
    start_height: Option<u32>,
    /// Parallel block fetchers.
    #[arg(long, default_value_t = 8)]
    fetch_workers: usize,
    /// RocksDB block cache, MiB.
    #[arg(long, default_value_t = 1024)]
    cache_mb: usize,
    /// Stop (and exit 0) once this height is indexed — for benchmarks and tests.
    #[arg(long)]
    stop_height: Option<u32>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Compare the UTXOs of N sampled indexed addresses with the node's
    /// `scantxoutset`; safe while the index is running. Exit 0 if they all
    /// match, 1 otherwise.
    Verify {
        #[arg(long, default_value_t = 100)]
        sample: usize,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(Cmd::Verify { sample }) = args.cmd {
        return match verify_cmd(&args, sample) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if is_fatal(&e) => {
            eprintln!("fatal: {e:#}");
            ExitCode::from(EXIT_FATAL)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `verify`: print every mismatch; `Ok(true)` when there are none.
fn verify_cmd(args: &Args, sample: usize) -> Result<bool> {
    let (rpc, _) = connect(args)?;
    let mismatches = verify::run(&args.db, &rpc, network(&args.network), sample)?;
    for m in &mismatches {
        println!("{m}");
    }
    eprintln!("verify: {} mismatch(es)", mismatches.len());
    Ok(mismatches.is_empty())
}

fn network(name: &str) -> bitcoin::Network {
    if regtest(name) {
        bitcoin::Network::Regtest
    } else {
        bitcoin::Network::Bitcoin
    }
}

fn regtest(network: &str) -> bool {
    network.starts_with("regtest")
}

fn connect(args: &Args) -> Result<(Rpc, String)> {
    let url = args.rpc_url.clone().unwrap_or_else(|| {
        if regtest(&args.network) {
            "http://127.0.0.1:18443"
        } else {
            "http://127.0.0.1:8332"
        }
        .into()
    });
    let rpc = match (&args.rpc_auth, &args.cookie_file) {
        (Some(auth), _) => Rpc::new(&url, auth.trim()),
        (None, Some(cf)) => Rpc::from_cookie_file(&url, cf)?,
        (None, None) => {
            let dd = args.datadir.clone().unwrap_or_else(default_bitcoin_datadir);
            let dd = if regtest(&args.network) {
                dd.join("regtest")
            } else {
                dd
            };
            Rpc::from_cookie_file(&url, &dd.join(".cookie"))?
        }
    };
    Ok((rpc, url))
}

fn run(args: Args) -> Result<()> {
    let (rpc, url) = connect(&args)?;
    let network = network(&args.network);
    let start_height = args.start_height.unwrap_or(if regtest(&args.network) {
        0
    } else {
        SEGWIT_ACTIVATION_HEIGHT
    });

    let cs = fortis_node::chain_status(&rpc).context("reaching the node")?;
    let fmt = HeaderFormat::detect(&rpc)?;
    let chain_id = fmt.chain_id();
    eprintln!(
        "fortis-index → {url}  ({}, chain {}, {chain_id})",
        cs.subversion, cs.chain
    );
    eprintln!(
        "             db {}  ·  indexing from height {start_height}",
        args.db.display()
    );

    let db = Db::open(
        &args.db,
        &DbConfig {
            cache_mb: args.cache_mb,
        },
        &chain_id,
    )?;
    let table = BlockTable::from_db(db.load_blocks()?)?;
    let table_tip = table.tip().map(|(h, r)| (h, r.hash));
    if db.reader().tip()? != table_tip {
        bail!(
            "index {} is inconsistent: meta.tip does not match the blocks table",
            args.db.display()
        );
    }
    let chain: SharedChain = Arc::new(ArcSwap::from_pointee(table));
    let mempool: SharedMempool = Arc::new(ArcSwap::from_pointee(MempoolView::default()));
    let status = Arc::new(SyncStatus::default());
    let src = Arc::new(RpcSource::new(rpc.clone()));
    // Calls made for an API client get a short timeout: a wedged node must not
    // hold a request (and its concurrency slot) for the syncer's 10 minutes.
    let api_rpc = rpc.with_timeout(std::time::Duration::from_secs(30));
    let renderer = Arc::new(Renderer::new(
        Arc::new(RpcSource::new(api_rpc.clone())) as Arc<dyn TxSource>,
        db.clone(),
        network,
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let sync_thread = {
        let mut syncer = Syncer::new(
            db.clone(),
            src.clone() as Arc<dyn BlockSource>,
            fmt,
            chain.clone(),
            start_height,
            FetchConfig {
                workers: args.fetch_workers.max(1),
            },
            args.stop_height,
            Backoff::default(),
            status.clone(),
        );
        let (stop, mempool, renderer, src) =
            (stop.clone(), mempool.clone(), renderer.clone(), src.clone());
        std::thread::Builder::new()
            .name("sync".into())
            .spawn(move || {
                let mut tracker = MempoolTracker::default();
                let mut after_pass = || match tracker.refresh(&*src) {
                    Ok(Some(v)) => {
                        mempool.store(Arc::new(v));
                        renderer.forget_pending_not_in(&mempool.load());
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("mempool: {e:#}"),
                };
                let r = syncer.run(&stop, &mut after_pass);
                let _ = done_tx.send(r);
            })
            .context("spawning sync thread")?
    };

    let state = api::AppState {
        db: db.clone(),
        chain,
        mempool,
        renderer,
        rpc: api_rpc,
        network,
        status,
        chain_id,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let sync_result = rt.block_on(serve(&args.bind, state, done_rx))?;

    stop.store(true, Ordering::SeqCst);
    if sync_thread.join().is_err() && sync_result.is_ok() {
        bail!("sync thread panicked");
    }
    db.flush()?;
    eprintln!("fortis-index: stopped");
    sync_result
}

/// Serve the API until a shutdown signal, or until the sync thread ends
/// (`--stop-height` reached, or a fatal error). Returns the sync result, or
/// `Ok` when stopped by a signal.
async fn serve(
    bind: &str,
    state: api::AppState,
    done: tokio::sync::oneshot::Receiver<Result<()>>,
) -> Result<Result<()>> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    eprintln!("fortis-index listening on  http://{bind}");
    let (end_tx, end_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        let mut end_rx = end_rx;
        axum::serve(listener, api::router(state))
            .with_graceful_shutdown(async move {
                let _ = end_rx.wait_for(|e| *e).await;
            })
            .await
    });
    let outcome = tokio::select! {
        _ = shutdown_signal() => {
            eprintln!("fortis-index: shutting down");
            Ok(())
        }
        r = done => sync_outcome(r),
    };
    let _ = end_tx.send(true);
    server.await?.context("HTTP server")?;
    Ok(outcome)
}

/// The sync thread's result; a thread that ended without sending one
/// panicked, which must not look like a clean stop (exit 0 = no restart).
fn sync_outcome(r: Result<Result<()>, tokio::sync::oneshot::error::RecvError>) -> Result<()> {
    r.unwrap_or_else(|_| Err(anyhow::anyhow!("sync thread panicked")))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_thread_that_died_without_a_result_is_an_error() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
        drop(tx); // the thread panicked before sending
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let r = rt.block_on(rx);
        assert!(sync_outcome(r).is_err());
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
        tx.send(Ok(())).unwrap();
        assert!(sync_outcome(rt.block_on(rx)).is_ok());
    }
}
