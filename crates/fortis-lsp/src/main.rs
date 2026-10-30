//! `fortis-lsp`: ldk-node in LSPS2-service mode — the fortis wallet's one
//! Lightning peer, opening zero-conf JIT channels for its clients.
//!
//! Usage: `fortis-lsp [--config /etc/fortis/lsp.toml]`.
//!
//! On start it prints, on stdout, one `key value` line each:
//! `node_id <hex>`, `listen <addr>`, `metrics <addr>`, `onchain_address <addr>`
//! (a fresh address of its on-chain wallet, for funding), then `ready`.
//! Diagnostics go to stderr; ldk-node's own log is `<data_dir>/logs/`.
//! SIGTERM / SIGINT stop the node cleanly (exit 0).

mod config;
mod metrics;

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::LspConfig;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Event};
use metrics::{Counters, Snapshot};

/// How often the on-chain balance is checked against `low_liquidity_sat`.
const LIQUIDITY_CHECK: Duration = Duration::from_secs(60);
/// ldk-node's seed file name in `data_dir` (its builder default).
const SEED_FILE: &str = "keys_seed";

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    // Only an atomic store: async-signal-safe.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: `on_signal` only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fortis-lsp: {e}");
            ExitCode::FAILURE
        }
    }
}

fn config_path() -> Result<String, String> {
    let mut args = std::env::args().skip(1);
    let mut path = config::DEFAULT_CONFIG_PATH.to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => path = args.next().ok_or("--config needs a path")?,
            "-h" | "--help" => {
                println!(
                    "usage: fortis-lsp [--config {}]",
                    config::DEFAULT_CONFIG_PATH
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?} (see --help)")),
        }
    }
    Ok(path)
}

fn run() -> Result<(), String> {
    install_signal_handlers();
    let path = config_path()?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let cfg = LspConfig::from_toml(&text)?;
    let rpc_password = cfg.rpc_password()?;
    let (rpc_host, rpc_port) = cfg.rpc_host_port()?;

    prepare_data_dir(&cfg)?;
    // Bind before starting the node so a taken port fails fast.
    let server = tiny_http::Server::http(cfg.metrics_listen)
        .map_err(|e| format!("metrics_listen {}: {e}", cfg.metrics_listen))?;
    let metrics_addr = server
        .server_addr()
        .to_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|| cfg.metrics_listen.to_string());

    let (min_payment_msat, max_payment_msat) = cfg.payment_bounds();
    eprintln!(
        "fortis-lsp: JIT payments {min_payment_msat}..={max_payment_msat} msat (configured {}..={}), keeping channels within {}..={} sat",
        cfg.min_payment_size_msat, cfg.max_payment_size_msat, cfg.channel_min_sat, cfg.channel_max_sat
    );

    let mut b = Builder::new();
    b.set_network(cfg.network);
    b.set_chain_source_bitcoind_rpc(rpc_host, rpc_port, cfg.rpc_user.clone(), rpc_password);
    let data_dir = cfg.data_dir.to_string_lossy().into_owned();
    b.set_storage_dir_path(data_dir.clone());
    // Generated (random, mode 0400) on first start, reused after: same node id.
    b.set_entropy_seed_path(cfg.data_dir.join(SEED_FILE).to_string_lossy().into_owned());
    let listen = SocketAddress::from(cfg.listen);
    b.set_listening_addresses(vec![listen])
        .map_err(|e| format!("listen {}: {e}", cfg.listen))?;
    b.set_liquidity_provider_lsps2(cfg.lsps2_service_config());
    let node = Arc::new(b.build().map_err(|e| format!("build node: {e}"))?);
    check_seed_permissions(&cfg)?;
    node.start().map_err(|e| format!("start node: {e}"))?;

    let result = serve(&cfg, &node, server, &metrics_addr);
    eprintln!("fortis-lsp: stopping");
    let stopped = node.stop().map_err(|e| format!("stop node: {e}"));
    result.and(stopped)?;
    eprintln!("fortis-lsp: stopped");
    Ok(())
}

/// Runs until SIGTERM/SIGINT: prints the startup lines, serves metrics,
/// handles node events, checks liquidity.
fn serve(
    cfg: &LspConfig,
    node: &Arc<ldk_node::Node>,
    server: tiny_http::Server,
    metrics_addr: &str,
) -> Result<(), String> {
    let address = node
        .onchain_payment()
        .new_address()
        .map_err(|e| format!("new address: {e}"))?;
    let info = serde_json::json!({
        "node_id": node.node_id().to_string(),
        "network": cfg.network.to_string(),
        "listen": cfg.listen.to_string(),
        "open_fee_ppm": cfg.open_fee_ppm,
        "open_fee_min_sat": cfg.open_fee_min_sat,
        // Effective bounds (config::effective_payment_bounds), not the raw
        // config: these keep every JIT channel within channel_min/max_sat.
        "min_payment_size_msat": cfg.payment_bounds().0,
        "max_payment_size_msat": cfg.payment_bounds().1,
        "channel_size_factor": cfg.channel_size_factor,
        "channel_min_sat": cfg.channel_min_sat,
        "channel_max_sat": cfg.channel_max_sat,
        "token_required": cfg.token.is_some(),
    })
    .to_string();

    let counters = Arc::new(Counters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let metrics_thread = {
        let (node, counters, stop) = (Arc::clone(node), Arc::clone(&counters), Arc::clone(&stop));
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || metrics::serve(server, node, counters, info, stop))
            .map_err(|e| format!("spawn metrics: {e}"))?
    };

    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "node_id {}", node.node_id());
        let _ = writeln!(out, "listen {}", cfg.listen);
        let _ = writeln!(out, "metrics {metrics_addr}");
        let _ = writeln!(out, "onchain_address {address}");
        let _ = writeln!(out, "ready");
        let _ = out.flush();
    }

    let mut next_check = Instant::now() + Duration::from_secs(5);
    while !SHUTDOWN.load(Ordering::SeqCst) {
        while let Some(event) = node.next_event() {
            handle_event(&event, &counters);
            if let Err(e) = node.event_handled() {
                eprintln!("fortis-lsp: event_handled: {e}");
                break;
            }
        }
        if Instant::now() >= next_check {
            next_check = Instant::now() + LIQUIDITY_CHECK;
            let onchain = Snapshot::read(node, &counters).onchain_sat;
            if metrics::low_liquidity(onchain, cfg.low_liquidity_sat) {
                eprintln!(
                    "fortis-lsp: WARNING low liquidity: on-chain {onchain} sat < low_liquidity_sat {}",
                    cfg.low_liquidity_sat
                );
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, Ordering::SeqCst);
    let _ = metrics_thread.join();
    Ok(())
}

fn handle_event(event: &Event, counters: &Counters) {
    match event {
        Event::PaymentForwarded { .. } => {
            counters.forwards.fetch_add(1, Ordering::Relaxed);
        }
        Event::ChannelPending {
            channel_id,
            counterparty_node_id,
            ..
        } => {
            eprintln!("fortis-lsp: channel {channel_id} pending with {counterparty_node_id}");
        }
        Event::ChannelReady {
            channel_id,
            counterparty_node_id,
            ..
        } => {
            let peer = counterparty_node_id
                .map(|p| p.to_string())
                .unwrap_or_default();
            eprintln!("fortis-lsp: channel {channel_id} ready with {peer}");
        }
        Event::ChannelClosed {
            channel_id, reason, ..
        } => {
            eprintln!("fortis-lsp: channel {channel_id} closed: {reason:?}");
        }
        _ => {}
    }
}

/// Creates `data_dir` (0700) if missing; refuses one others can read.
fn prepare_data_dir(cfg: &LspConfig) -> Result<(), String> {
    let dir = &cfg.data_dir;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("data_dir {}: {e}", dir.display()))?;
    }
    let mode = std::fs::metadata(dir)
        .map_err(|e| format!("data_dir {}: {e}", dir.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "data_dir {} is accessible to group/others (mode {:o}); chmod 700 it",
            dir.display(),
            mode & 0o777
        ));
    }
    Ok(())
}

/// The seed file ldk-node created (0400) or found must not be readable by
/// anyone but us.
fn check_seed_permissions(cfg: &LspConfig) -> Result<(), String> {
    let seed = cfg.data_dir.join(SEED_FILE);
    let mode = std::fs::metadata(&seed)
        .map_err(|e| format!("{}: {e}", seed.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is accessible to group/others (mode {:o}); chmod 400 it",
            seed.display(),
            mode & 0o777
        ));
    }
    Ok(())
}
