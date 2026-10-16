//! fortis-edge — the public front for the fortis wallet backends.
//!
//! It sits in front of a `fortis-index` instance (XBT) and an Esplora upstream
//! (BTC — a public explorer, or a `fortisd --esplora-proxy`) and adds what a
//! backend exposed to many wallets needs: per-install tokens, per-key rate
//! limiting, short-TTL response caching, locked-down CORS, and `/metrics`.
//! Stateless (HMAC tokens, in-memory limiter/cache) so instances scale out.
//!
//! TLS is expected from a reverse proxy (Caddy / nginx) in front.

mod btc_history;
mod cache;
mod haskoin;
mod fx;
mod limit;
mod mempool_ws;
mod metrics;
mod price;
mod pricing;
mod proxy;
mod rdb;
mod reconcile;
mod scan;
mod scan_cache;
mod swap;
mod token;

#[cfg(test)]
pub(crate) mod test_tmp {
    //! Temp directories for tests that open RocksDB stores: each one is
    //! removed on drop, so `cargo test` leaves nothing behind under /tmp.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh path `fortis-edge-{label}-{pid}-{n}` under the system temp
    /// dir (not created: the store opening it does that), removed with
    /// everything in it on drop. Declare it before whatever opens a store in
    /// it, so the store is closed first.
    pub struct TempDir(PathBuf);

    /// Remove `fortis-edge-…-{pid}-{n}` dirs whose process is gone: what a
    /// crashed or killed run left behind (a full /tmp quota makes RocksDB fail).
    pub fn sweep_dead() {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("fortis-edge-") else { continue };
            let mut parts = rest.rsplitn(3, '-');
            let (Some(n), Some(pid), Some(_label)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let (Ok(_), Ok(pid)) = (n.parse::<u64>(), pid.parse::<u32>()) else { continue };
            if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            static SWEEP: std::sync::Once = std::sync::Once::new();
            SWEEP.call_once(sweep_dead);
            static NEXT: AtomicU64 = AtomicU64::new(0);
            TempDir(std::env::temp_dir().join(format!(
                "fortis-edge-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A run that crashed (or was killed) never dropped its dirs; the next
    /// run removes them — dirs of processes that are gone, never a live one's.
    #[test]
    fn dirs_left_by_a_dead_run_are_swept() {
        let base = std::env::temp_dir();
        // 2^22 = the largest pid_max Linux allows: pids are always below it
        let dead = base.join("fortis-edge-sweep-test-4194304-0");
        let live = base.join(format!("fortis-edge-sweep-test-{}-999", std::process::id()));
        for d in [&dead, &live] {
            std::fs::create_dir_all(d.join("db")).unwrap();
        }
        sweep_dead();
        assert!(!dead.exists(), "a dead run's dir is removed");
        assert!(live.exists(), "this run's own dir is kept");
        std::fs::remove_dir_all(&live).unwrap();
    }

    #[test]
    fn a_temp_dir_is_removed_on_drop() {
        let d = TempDir::new("tmp-guard-test");
        std::fs::create_dir_all(d.join("db")).unwrap();
        std::fs::write(d.join("db").join("f"), b"x").unwrap();
        let path = d.to_path_buf();
        assert!(path.exists());
        drop(d);
        assert!(!path.exists());
    }
}

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server};

use btc_history::BtcHistory;
use cache::Cache;
use limit::{Pacer, RateLimiter};
use metrics::Metrics;
use proxy::Upstream;

#[derive(Parser)]
#[command(name = "fortis-edge", version, about = "public edge for the fortis wallet backends")]
struct Args {
    /// Address to bind to.
    #[arg(long, default_value = "127.0.0.1:8098")]
    bind: String,
    /// XBT upstream — a `fortis-index` base URL, e.g. http://127.0.0.1:8094.
    #[arg(long)]
    xbt_upstream: Option<String>,
    /// BTC upstream — an Esplora base URL, e.g. https://mempool.space/api or
    /// http://127.0.0.1:8088/esplora.
    #[arg(long)]
    btc_upstream: Option<String>,
    /// Extra BTC upstream(s), tried in order for a request that fails against
    /// `--btc-upstream` (429 after retries, 5xx, or unreachable), and against
    /// each other in turn. Repeat the flag to list more than one. Different
    /// providers so one explorer rate-limiting or dropping *this edge's own
    /// egress IP* doesn't take every BTC wallet behind this edge down with
    /// it — every client's address lookups funnel through that one IP, so
    /// the explorer sees them as a single very busy caller regardless of how
    /// many distinct end users there are. Confirmed live, 2026-09-15:
    /// blockstream.info (the deployed `--btc-upstream`) started 429ing this
    /// machine's IP outright after a day of wallet-scan testing, while
    /// mempool.space kept answering normally throughout — this default just
    /// codifies that as a standing fallback chain. Pass an empty string alone
    /// to disable.
    #[arg(long, default_values_t = vec!["https://mempool.space/api".to_string()])]
    btc_upstream_fallback: Vec<String>,
    /// Maestro (gomaestro.org) API key. When set, its Esplora-compatible BTC
    /// endpoint (`https://xbt-mainnet.gomaestro-api.org/v0/esplora`, same
    /// `/address/{addr}/utxo|txs` shape as the rest of the chain) is appended
    /// as the last fallback — a paid, per-key-metered provider rather than a
    /// shared public explorer, so it isn't exposed to other users' traffic
    /// tripping a shared IP's rate limit the way `--btc-upstream-fallback`'s
    /// public explorers are. Kept last since it's metered: only reached once
    /// the free options have all failed.
    #[arg(long)]
    btc_maestro_key: Option<String>,
    /// USD price source for `GET /btc/v1/prices`. A full URL the edge fetches and
    /// normalises to `{ "USD": <n> }` — a mempool `/v1/prices` endpoint or a
    /// Kraken-style `Ticker` (e.g.
    /// `https://api.kraken.com/0/public/Ticker?pair=XBTUSD`). Unset → the route
    /// proxies to `--btc-upstream/v1/prices` as before.
    #[arg(long)]
    btc_price_url: Option<String>,
    /// USD price source for `GET /xbt/v1/prices` — same rules as
    /// `--btc-price-url`. A `fortis-index` (`--xbt-upstream`) has no price feed,
    /// so without this (or `--xbt-price-upstream`) the route 404s and the
    /// wallet shows no fiat value.
    #[arg(long)]
    xbt_price_url: Option<String>,
    /// Deprecated alias for `--xbt-price-url`: a mempool-style *base* URL whose
    /// `/v1/prices` carries the feed (e.g. https://mempool.kilombino.com/api).
    #[arg(long)]
    xbt_price_upstream: Option<String>,
    /// Exchange-rate feed for the non-USD currencies in `/v1/prices`: the ECB
    /// daily reference rates XML
    /// (https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml).
    /// Unset → `/v1/prices` answers USD only. Only applies to a chain with a
    /// price source (`--{btc,xbt}-price-url`).
    #[arg(long)]
    fx_url: Option<String>,
    /// Live exchange rates, used first: Coinbase's public
    /// `https://api.coinbase.com/v2/exchange-rates?currency=USD` (no key,
    /// updated every minute, asked every 10 min). A currency falls back to
    /// `--fx-url`'s ECB rate only when this fails for it — see `fx.rs`.
    #[arg(long)]
    fx_live_url: Option<String>,
    /// Cap on `/btc/address/*` lookups per second sent to `--btc-upstream`.
    /// A wallet scan fans out ~40 of them at once and public explorers
    /// (mempool.space, blockstream.info) 429 the burst; the edge queues them
    /// instead. `0` disables. Only affects BTC — a `fortis-index` has no limit.
    #[arg(long, default_value_t = 5.0)]
    btc_upstream_rate: f64,
    /// Batch source for BTC address data — a Haskoin Store base URL. Enables
    /// `POST /btc/prewarm` and the BTC branch of `POST /btc/scan`, which fetch
    /// a wallet's whole address set in one or two calls instead of fanning out
    /// to `--btc-upstream` per address. Unset (default) disables both — `/btc/
    /// scan` 404s for BTC, which the Android/web clients already treat as
    /// "unsupported" and fall back to their per-address walk against
    /// `--btc-upstream`/`--btc-upstream-fallback`.
    ///
    /// Was on by default (api.haskoin.com) until 2026-09-28: confirmed live,
    /// it started 503ing (rate-limited/unavailable) on real wallet-sized batch
    /// requests, turning into a hard 502 for every scan instead of a graceful
    /// degrade — worse than not having it. Deliberately an `Option<String>`
    /// with no default now, not a `String` with an empty-string-means-off
    /// convention: passing an empty string through nssm's `AppParameters`
    /// (a single string re-split by Windows on service start) turned out to
    /// not survive the round-trip intact and silently merged every following
    /// flag into this one's value — omitting the flag entirely, which this
    /// type makes the natural way to disable it, has no such failure mode.
    #[arg(long)]
    btc_haskoin_url: Option<String>,
    /// Optional `X-API-Key` header for `--btc-haskoin-url` (api.haskoin.com needs
    /// none).
    #[arg(long)]
    btc_haskoin_key: Option<String>,
    /// `--btc-upstream` is a `fortis-index` (e.g. one over a local Bitcoin
    /// Core node), so forward `POST /btc/scan` to it exactly as `/xbt/scan`
    /// is forwarded — answered natively from its address index. Takes
    /// precedence over `--btc-haskoin-url` for `/btc/scan`.
    #[arg(long)]
    btc_upstream_scan: bool,
    /// Broadcast `POST /btc/tx` through a local Bitcoin Core / Knots node's
    /// `sendrawtransaction` instead of `--btc-upstream` (a pruned node is fine).
    /// Lets replay-protected sends (oversized `OP_RETURN`) reach the network even
    /// when the Esplora upstream won't relay them — the node still has to accept
    /// them (Core 30+, or `-datacarriersize` raised). Address / history / fee
    /// reads still use `--btc-upstream`.
    #[arg(long, value_name = "URL")]
    btc_rpc_url: Option<String>,
    /// `user:pass` for `--btc-rpc-url` (a bitcoin.conf `rpcauth` line).
    #[arg(long, value_name = "USER:PASS")]
    btc_rpc_auth: Option<String>,
    /// RPC cookie file for `--btc-rpc-url` (alternative to `--btc-rpc-auth`).
    #[arg(long, value_name = "FILE")]
    btc_rpc_cookie: Option<PathBuf>,
    /// HMAC secret file for tokens. Default: <home>/fortis-edge.secret.
    #[arg(long)]
    secret_file: Option<PathBuf>,
    /// Reject `/btc/*` and `/xbt/*` without a valid `Authorization: Bearer`
    /// (or `?token=`) minted by `POST /register`.
    #[arg(long)]
    require_token: bool,
    /// Trust `X-Forwarded-For` for the client IP. Only enable behind a proxy that
    /// sets it and strips any inbound value.
    #[arg(long)]
    trust_forwarded_for: bool,
    /// CORS `Access-Control-Allow-Origin`.
    #[arg(long, default_value = "*")]
    allow_origin: String,
    /// Sustained requests per minute, per token (or per IP if untokened). A
    /// wallet refresh is one `POST /{chain}/scan` however many addresses it
    /// covers; the per-address routes (public-explorer-style clients) cost
    /// ~2 requests per address and repeat on every refresh.
    #[arg(long, default_value_t = 600)]
    rate_per_min: u32,
    /// Rate-limit bucket capacity (burst). Must cover a whole gap-limit scan
    /// (tip + utxo + txs per address) landing at once.
    #[arg(long, default_value_t = 300)]
    rate_burst: u32,
    /// `POST /register` calls allowed per hour, per client IP.
    #[arg(long, default_value_t = 10)]
    register_per_hour: u32,
    /// Max cached responses, in memory and in the RocksDB directory backing it.
    #[arg(long, default_value_t = 4096)]
    cache_entries: usize,
    /// RocksDB directory the response cache persists to, so a restart doesn't
    /// throw away already-fetched (and, for confirmed transaction history,
    /// unchanging) data. Default: <home>/fortis-edge-cache-rocksdb.
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// RocksDB directory the permanent confirmed-BTC-history store persists
    /// to. Separate from `--cache-dir` (different lifetime/eviction policy —
    /// see `btc_history`'s doc comment). Default:
    /// <home>/fortis-edge-btc-history-rocksdb.
    #[arg(long)]
    btc_history_dir: Option<PathBuf>,
    /// RocksDB directory the BTC batch-scan skip-cache persists to — lets
    /// `POST /btc/scan` skip re-fetching an address's full transaction
    /// history from Haskoin when nothing about it has changed since the
    /// last scan (see `scan_cache`'s doc comment). Default:
    /// <home>/fortis-edge-btc-scancache-rocksdb.
    #[arg(long)]
    btc_scan_cache_dir: Option<PathBuf>,
    /// A mempool.space-compatible WebSocket URL (`.../api/v1/ws`) to track
    /// scanned addresses on for low-latency cache invalidation — purely an
    /// optimization layered on the existing Haskoin+scan-cache path, see
    /// `mempool_ws`'s doc comment. mempool.space itself is unreachable from
    /// this network (confirmed 2026-09-28); a good default once enabled is
    /// a community-run mirror on the same open-source backend, e.g.
    /// `wss://mempool.emzy.de/api/v1/ws`. Unset (default) disables this
    /// entirely — no outbound connection is made.
    #[arg(long)]
    btc_mempool_ws_url: Option<String>,
    /// HTTP worker threads. Long-polls may hold half of them, and an
    /// upstream call can take seconds: threads are cheap, waiting isn't.
    #[arg(long, default_value_t = 32)]
    workers: usize,
    /// Append `POST /crash` reports to this file as NDJSON (one JSON object per
    /// line). Unset → `/crash` returns 404.
    #[arg(long)]
    crash_log: Option<PathBuf>,
    /// Network the fee address is on: `bitcoin` (default), `testnet`, `signet`,
    /// `regtest`. Only used to validate `--service-fee-address`.
    #[arg(long, default_value = "bitcoin")]
    network: bitcoin::Network,
    /// Service-fee address. When set, `GET /pricing` advertises the fee and
    /// `POST /<chain>/tx` is rejected unless the transaction pays at least
    /// `--service-fee-floor-sat` to this address. Unset → no fee, `/pricing` 404s.
    #[arg(long)]
    service_fee_address: Option<String>,
    /// Service fee, basis points of the amount sent — default 100 (1%).
    /// Advertised; the client adds the output, the edge enforces the floor.
    #[arg(long, default_value_t = 100)]
    service_fee_bps: u32,
    /// Minimum service fee per transaction, satoshis. This is what the edge
    /// enforces on broadcast. Keep it above the dust limit for the fee address
    /// (294 for a bech32 P2WPKH, 546 for legacy P2PKH) — a smaller output makes
    /// the whole transaction non-standard and the node rejects it.
    #[arg(long, default_value_t = 400)]
    service_fee_floor_sat: u64,
    /// Maximum service fee per transaction, satoshis (0 = uncapped). Advertised only.
    #[arg(long, default_value_t = 0)]
    service_fee_cap_sat: u64,
    /// Enable P2P BTC ⇄ XBT swaps (`/swap/*`). Needs `--service-fee-address`
    /// (the swap commission goes there) and a `--network` of `bitcoin` or
    /// `regtest`. Off → `/swap/*` 404s. Single instance only: the swap state is
    /// one local RocksDB.
    #[arg(long)]
    swap: bool,
    /// RocksDB directory for swap offers, matches, relay messages and watch jobs.
    /// Default: <home>/fortis-edge-swap-rocksdb.
    #[arg(long)]
    swap_db: Option<PathBuf>,
    /// Swap commission per side, basis points. The protocol fixes it at 50;
    /// phones reject terms with any other value.
    #[arg(long, default_value_t = 50)]
    swap_fee_bps: u32,
    /// Largest BTC leg of one swap, satoshis. Unset = no maximum.
    #[arg(long)]
    swap_max_sat: Option<u64>,
    /// NeoxEX base URL, the swap price source.
    #[arg(long, default_value = "https://neoxa.exchange")]
    neoxex_url: String,
    /// Test only: seconds between swap watchtower passes (default 60).
    #[arg(long, default_value_t = swap::watchtower::TICK.as_secs(), hide = true)]
    swap_tick_secs: u64,
    /// Test only: run the swap watchtower's clock this many seconds ahead of
    /// the wall clock, so an end-to-end test reaches refund times at once.
    #[arg(long, default_value_t = 0, hide = true)]
    swap_clock_offset_secs: u64,
}

struct State {
    secret: Vec<u8>,
    require_token: bool,
    trust_forwarded_for: bool,
    allow_origin: String,
    xbt: Option<Upstream>,
    btc: Option<Upstream>,
    /// Tried in order after `btc` fails; see `--btc-upstream-fallback` /
    /// `--btc-maestro-key`.
    btc_fallbacks: Vec<Upstream>,
    btc_tip: Option<proxy::TipCache>,
    btc_broadcast: Option<fortis_node::Rpc>,
    btc_price: Option<price::PriceCache>,
    xbt_price: Option<price::PriceCache>,
    fx: Option<fx::FxCache>,
    btc_haskoin: Option<haskoin::HaskoinStore>,
    btc_upstream_scan: bool,
    btc_pacer: Option<Pacer>,
    limiter: RateLimiter,
    register_limiter: RateLimiter,
    crash_limiter: RateLimiter,
    crash_log: Option<PathBuf>,
    pricing: Option<pricing::Pricing>,
    cache: Cache,
    /// Permanent, never-evicted confirmed-tx store for BTC — see
    /// `btc_history`'s doc comment. `None` only if the db couldn't be opened.
    btc_history: Option<BtcHistory>,
    /// Lets a BTC batch scan skip re-fetching an address's full history when
    /// nothing about it has changed since last time — see `scan_cache`'s doc
    /// comment. `None` only if the db couldn't be opened.
    btc_scan_cache: Option<scan_cache::ScanCache>,
    /// Optional low-latency companion to `btc_scan_cache` — see
    /// `mempool_ws`'s doc comment. `None` unless `--btc-mempool-ws-url` is set.
    btc_mempool_ws: Option<mempool_ws::MempoolWs>,
    metrics: Metrics,
    /// P2P swaps; `None` unless `--swap`.
    swap: Option<swap::Swap>,
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

fn default_home() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("fortis-edge")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".fortis-edge")
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    if args.xbt_upstream.is_none() && args.btc_upstream.is_none() {
        return Err(anyhow!("set at least one of --xbt-upstream / --btc-upstream"));
    }
    let secret_file = args
        .secret_file
        .clone()
        .unwrap_or_else(|| default_home().join("fortis-edge.secret"));
    let secret = token::load_or_create_secret(&secret_file)?;
    let cache_dir =
        args.cache_dir.clone().unwrap_or_else(|| default_home().join("fortis-edge-cache-rocksdb"));
    let btc_history_dir = args
        .btc_history_dir
        .clone()
        .unwrap_or_else(|| default_home().join("fortis-edge-btc-history-rocksdb"));
    let btc_scan_cache_dir = args
        .btc_scan_cache_dir
        .clone()
        .unwrap_or_else(|| default_home().join("fortis-edge-btc-scancache-rocksdb"));

    // `--xbt-price-url` wins; `--xbt-price-upstream <base>` is the old form
    // that pointed at a mempool base and implied `/v1/prices`.
    let xbt_price_url = args.xbt_price_url.clone().or_else(|| {
        args.xbt_price_upstream
            .as_deref()
            .map(|b| format!("{}/v1/prices", b.trim_end_matches('/')))
    });

    let fee = match &args.service_fee_address {
        Some(addr) => Some(pricing::Pricing::new(
            addr.clone(),
            args.network,
            args.service_fee_bps,
            args.service_fee_floor_sat,
            args.service_fee_cap_sat,
        )?),
        None => None,
    };

    let swap = if args.swap { Some(build_swap(&args, &secret)?) } else { None };

    let btc_broadcast = match &args.btc_rpc_url {
        None => None,
        Some(url) => {
            let auth = match (&args.btc_rpc_auth, &args.btc_rpc_cookie) {
                (Some(a), _) => a.trim().to_string(),
                (None, Some(cf)) => std::fs::read_to_string(cf)
                    .with_context(|| format!("reading {}", cf.display()))?
                    .trim()
                    .to_string(),
                (None, None) => {
                    return Err(anyhow!("--btc-rpc-url needs --btc-rpc-auth or --btc-rpc-cookie"))
                }
            };
            // Short timeout: broadcast + reachability pings should be instant on a
            // local node; if it's wedged we fall back to --btc-upstream fast.
            let rpc = fortis_node::Rpc::new_with_timeout(
                url,
                &auth,
                std::time::Duration::from_secs(20),
            );
            match fortis_node::chain_status(&rpc) {
                Ok(cs) => eprintln!(
                    "  btc broadcast  node {url}  ({}, chain {})  [fallback: --btc-upstream]",
                    cs.subversion, cs.chain
                ),
                Err(e) => eprintln!(
                    "  btc broadcast  node {url} unreachable at startup ({e}); \
                     will retry per request, fall back to --btc-upstream"
                ),
            }
            Some(rpc)
        }
    };

    // Maestro (when a key is given) goes first, ahead of --btc-upstream: it's
    // a paid, per-key-metered provider rather than a shared public explorer,
    // so it isn't exposed to a public IP-wide rate limit or the public
    // explorers' own reliability wobbles (confirmed live, 2026-09-15:
    // blockstream.info 429ing outright and mempool.space hanging entirely,
    // while Maestro answered in under 300ms) — worth trying first, not last,
    // once it's configured. Demotes the configured --btc-upstream to the
    // first fallback rather than dropping it, so a Maestro outage still
    // recovers instead of losing BTC entirely.
    let maestro = args.btc_maestro_key.as_deref().map(|key| {
        (
            "https://xbt-mainnet.gomaestro-api.org/v0/esplora",
            ("api-key".to_string(), key.to_string()),
        )
    });
    // The full ordered BTC chain (Maestro first when configured, then
    // --btc-upstream, then --btc-upstream-fallback) as fresh `Upstream`
    // instances — called once for the per-request `btc`/`btc_fallbacks`
    // split and again for `TipCache`, since `Upstream` isn't `Clone` and both
    // need their *own* chain, not a shared one, to fail over independently.
    let build_btc_chain = || -> Vec<Upstream> {
        let mut chain = Vec::new();
        if let Some((url, header)) = &maestro {
            chain.push(Upstream::with_header(url, Some(header.clone())));
        }
        if let Some(primary) = &args.btc_upstream {
            chain.push(if maestro.is_some() { Upstream::fallback(primary, None) } else { Upstream::new(primary) });
        }
        chain.extend(
            args.btc_upstream_fallback
                .iter()
                .map(|u| u.trim())
                .filter(|u| !u.is_empty() && Some(*u) != args.btc_upstream.as_deref())
                .map(|u| Upstream::fallback(u, None)),
        );
        chain
    };
    let (btc, btc_fallbacks): (Option<Upstream>, Vec<Upstream>) = {
        let mut chain = build_btc_chain();
        if chain.is_empty() {
            (None, Vec::new())
        } else {
            let rest = chain.split_off(1);
            (chain.into_iter().next(), rest)
        }
    };
    // Same chain as `build_btc_chain`, minus Maestro: the tip height changes
    // roughly every 10 minutes on mainnet, and this cache polls every 20s
    // regardless of success, so there's no latency case for spending a paid,
    // per-key-metered provider's quota on it. Live-measured on Maestro's own
    // dashboard, 2026-09-17: this one endpoint was 61% of this key's total
    // query volume at a 34% success rate, well above what the address/tx/utxo
    // lookups Maestro is actually valuable for were getting through. Falls
    // back to `--btc-upstream-fallback` same as the real per-request chain if
    // configured, so it's still a real chain, just without the paid tier.
    let build_btc_tip_chain = || -> Vec<Upstream> {
        let mut chain = Vec::new();
        if let Some(primary) = &args.btc_upstream {
            let has_more = args.btc_upstream_fallback.iter().any(|u| {
                let u = u.trim();
                !u.is_empty() && Some(u) != args.btc_upstream.as_deref()
            });
            chain.push(if has_more { Upstream::fallback(primary, None) } else { Upstream::new(primary) });
        }
        chain.extend(
            args.btc_upstream_fallback
                .iter()
                .map(|u| u.trim())
                .filter(|u| !u.is_empty() && Some(*u) != args.btc_upstream.as_deref())
                .map(|u| Upstream::fallback(u, None)),
        );
        chain
    };

    // The swap watchtower reads and broadcasts through its own instances of
    // the same upstreams (`Upstream` isn't `Clone`; each chain fails over on
    // its own) and skips the `/tx` fee check — see `swap::watchtower`. The
    // watch route checks unrecorded HTLC outpoints through the same reader.
    let swap_reader: Option<Arc<dyn swap::watchtower::ChainReader>> = swap.is_some().then(|| {
        Arc::new(
            swap::watchtower::UpstreamReader::new(
                args.xbt_upstream.as_deref().map(Upstream::new),
                build_btc_chain(),
                btc_broadcast.clone(),
            )
            .with_clock_offset(args.swap_clock_offset_secs),
        ) as Arc<dyn swap::watchtower::ChainReader>
    });
    let swap = match (swap, &swap_reader) {
        (Some(sw), Some(r)) => Some(sw.with_chain_reader(Arc::clone(r))),
        (sw, _) => sw,
    };

    let state = Arc::new(State {
        secret,
        require_token: args.require_token,
        trust_forwarded_for: args.trust_forwarded_for,
        allow_origin: args.allow_origin.clone(),
        xbt: args.xbt_upstream.as_deref().map(Upstream::new),
        btc,
        btc_fallbacks,
        // XBT's tip comes from fortis-index (local, fast — no hang risk seen
        // there); only BTC's public-explorer proxy needs the background
        // cache. Deliberately *not* `build_btc_chain()` — see
        // `build_btc_tip_chain`'s doc for why Maestro is excluded here even
        // though it's the real per-request chain's primary. `btc_broadcast`
        // (the local node, when `--btc-rpc-url` is set) is tried before any
        // of that chain — see `TipCache::spawn`'s doc for why a node this
        // deployment already runs and trusts is a strictly better tip source
        // than any public explorer.
        btc_tip: {
            let chain = build_btc_tip_chain();
            (btc_broadcast.is_some() || !chain.is_empty())
                .then(|| proxy::TipCache::spawn(btc_broadcast.clone(), chain, std::time::Duration::from_secs(20)))
        },
        btc_broadcast,
        btc_price: args
            .btc_price_url
            .as_deref()
            .map(|u| price::PriceCache::spawn(price::PriceSource::new(u), std::time::Duration::from_secs(60))),
        xbt_price: xbt_price_url
            .as_deref()
            .map(|u| price::PriceCache::spawn(price::PriceSource::new(u), std::time::Duration::from_secs(60))),
        fx: (args.fx_url.is_some() || args.fx_live_url.is_some())
            .then(|| fx::FxCache::spawn(args.fx_live_url.as_deref(), args.fx_url.as_deref())),
        btc_haskoin: args
            .btc_haskoin_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(|u| haskoin::HaskoinStore::new(u, args.btc_haskoin_key.clone())),
        btc_upstream_scan: args.btc_upstream_scan,
        btc_pacer: (args.btc_upstream_rate > 0.0).then(|| Pacer::new(args.btc_upstream_rate)),
        limiter: RateLimiter::new(args.rate_per_min, args.rate_burst),
        register_limiter: RateLimiter::new(args.register_per_hour, args.register_per_hour.max(1)),
        // crashes are rare per device; this just caps a crash-looping client or abuse
        crash_limiter: RateLimiter::new(2, 8),
        crash_log: args.crash_log.clone(),
        pricing: fee,
        cache: Cache::new(args.cache_entries, &cache_dir),
        btc_history: BtcHistory::new(&btc_history_dir),
        btc_scan_cache: scan_cache::ScanCache::new(&btc_scan_cache_dir),
        btc_mempool_ws: args.btc_mempool_ws_url.clone().map(mempool_ws::MempoolWs::spawn),
        metrics: Metrics::default(),
        swap,
    });

    let server = Arc::new(
        Server::http(&args.bind).map_err(|e| anyhow!("cannot bind {}: {e}", args.bind))?,
    );

    eprintln!("fortis-edge listening on  http://{}", args.bind);
    eprintln!("  xbt upstream   {}", args.xbt_upstream.as_deref().unwrap_or("(none)"));
    eprintln!(
        "  btc upstream   {}",
        if args.btc_maestro_key.is_some() { "maestro (xbt-mainnet.gomaestro-api.org)" }
        else { args.btc_upstream.as_deref().unwrap_or("(none)") }
    );
    eprintln!(
        "  btc fallback   {}",
        if state.btc_fallbacks.is_empty() {
            "(none)".to_string()
        } else if args.btc_maestro_key.is_some() {
            args.btc_upstream.iter().chain(args.btc_upstream_fallback.iter()).cloned().collect::<Vec<_>>().join(", ")
        } else {
            args.btc_upstream_fallback.join(", ")
        }
    );
    eprintln!("  btc broadcast  {}", args.btc_rpc_url.as_deref().unwrap_or("(via btc upstream)"));
    eprintln!("  btc price      {}", args.btc_price_url.as_deref().unwrap_or("(via btc upstream)"));
    eprintln!("  xbt price    {}", xbt_price_url.as_deref().unwrap_or("(none)"));
    eprintln!("  fx live        {}", args.fx_live_url.as_deref().unwrap_or("(none)"));
    eprintln!("  fx fallback    {}", args.fx_url.as_deref().unwrap_or("(none)"));
    eprintln!("  cache dir      {}", cache_dir.display());
    eprintln!(
        "  btc history    {}",
        if state.btc_history.is_some() {
            format!("permanent ({})", btc_history_dir.display())
        } else {
            "(disabled)".to_string()
        }
    );
    eprintln!(
        "  btc scan cache {}",
        if state.btc_scan_cache.is_some() {
            format!("enabled ({})", btc_scan_cache_dir.display())
        } else {
            "(disabled)".to_string()
        }
    );
    eprintln!(
        "  btc mempool ws {}",
        args.btc_mempool_ws_url.as_deref().unwrap_or("(disabled)")
    );
    eprintln!(
        "  btc pacing     {}",
        if args.btc_upstream_rate > 0.0 {
            format!("{}/s on /address/*", args.btc_upstream_rate)
        } else {
            "(off)".into()
        },
    );
    if args.btc_upstream_scan {
        eprintln!("  btc scan       forwarded to btc upstream (fortis-index)");
    }
    eprintln!(
        "  btc batch      {}",
        match args.btc_haskoin_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(u) => format!("{u} (/btc/prewarm)"),
            None => "(off)".into(),
        },
    );
    eprintln!("  token auth     {}", if args.require_token { "required" } else { "optional" });
    eprintln!("  rate limit     {}/min, burst {}", args.rate_per_min, args.rate_burst);
    eprintln!("  crash log      {}", args.crash_log.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(disabled)".into()));
    eprintln!(
        "  service fee    {}",
        match &args.service_fee_address {
            Some(a) => format!(
                "{} bps, floor {} sat → {}",
                args.service_fee_bps, args.service_fee_floor_sat, a
            ),
            None => "(disabled)".into(),
        },
    );
    eprintln!(
        "  swaps          {}",
        if state.swap.is_some() {
            format!(
                "enabled ({}, max {}, neoxex {})",
                swap_db_dir(&args).display(),
                args.swap_max_sat.map_or("none".to_string(), |m| format!("{m} sat")),
                args.neoxex_url
            )
        } else {
            "(disabled)".into()
        },
    );

    if let (Some(sw), Some(reader)) = (&state.swap, swap_reader) {
        let tick = std::time::Duration::from_secs(args.swap_tick_secs.max(1));
        swap::watchtower::spawn(sw.store(), sw.matching(), reader, tick);
        eprintln!("  swap watchtower every {}s", tick.as_secs());
        if args.swap_clock_offset_secs > 0 {
            eprintln!("  swap watchtower clock +{}s (test only)", args.swap_clock_offset_secs);
        }
    }

    let mut handles = Vec::new();
    for _ in 0..args.workers.max(1) {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(thread::spawn(move || {
            for mut req in server.incoming_requests() {
                // One panicking request must not take the worker (and its share of
                // the pool) down with it — catch it, count it, answer 500.
                let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle(&mut req, &state)
                }))
                .unwrap_or_else(|_| {
                    Metrics::inc(&state.metrics.panics);
                    eprintln!("fortis-edge: request handler panicked; returned 500");
                    err(500, "internal error")
                });
                let _ = respond(req, reply, &state.allow_origin);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

fn swap_db_dir(args: &Args) -> PathBuf {
    args.swap_db.clone().unwrap_or_else(|| default_home().join("fortis-edge-swap-rocksdb"))
}

/// `token_key` is the install-token HMAC key; it also derives idempotent offers' owner secrets.
fn build_swap(args: &Args, token_key: &[u8]) -> Result<swap::Swap> {
    let Some(fee_address) = args.service_fee_address.clone() else {
        return Err(anyhow!(
            "--swap needs --service-fee-address: the swap commission is paid to it"
        ));
    };
    let network = match args.network {
        bitcoin::Network::Bitcoin => "bitcoin",
        bitcoin::Network::Regtest => "regtest",
        other => return Err(anyhow!("--swap supports --network bitcoin or regtest, not {other}")),
    };
    if args.swap_fee_bps != wallet_core::swap_terms::SWAP_FEE_BPS {
        return Err(anyhow!(
            "--swap-fee-bps must be {}: phones reject terms with any other fee",
            wallet_core::swap_terms::SWAP_FEE_BPS
        ));
    }
    if args.swap_max_sat.is_some_and(|m| m < wallet_core::swap_terms::MIN_BTC_LEG_SAT) {
        return Err(anyhow!(
            "--swap-max-sat must be at least the {} sat minimum swap",
            wallet_core::swap_terms::MIN_BTC_LEG_SAT
        ));
    }
    if args.service_fee_floor_sat > wallet_core::swap_terms::SWAP_FEE_FLOOR_SAT {
        return Err(anyhow!(
            "--service-fee-floor-sat must be at most {floor} with --swap: a swap funding pays the {floor} sat minimum commission and would be refused (402)",
            floor = wallet_core::swap_terms::SWAP_FEE_FLOOR_SAT
        ));
    }
    let fee_spk = fee_address
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .ok()
        .and_then(|a| a.require_network(args.network).ok())
        .ok_or_else(|| anyhow!("--service-fee-address is not a valid {network} address"))?
        .script_pubkey();
    let dust = fee_spk.minimal_non_dust().to_sat();
    if dust > wallet_core::swap_terms::SWAP_FEE_FLOOR_SAT {
        return Err(anyhow!(
            "--service-fee-address must be a SegWit address with --swap: its dust limit ({dust} sat) is above the {floor} sat minimum commission",
            floor = wallet_core::swap_terms::SWAP_FEE_FLOOR_SAT
        ));
    }
    let store = swap::store::Store::open(&swap_db_dir(args))?;
    Ok(swap::Swap::new(
        store,
        swap::neoxex::NeoxEx::new(args.neoxex_url.clone()),
        args.swap_fee_bps,
        args.swap_max_sat,
        network.to_string(),
        fee_address,
        token_key.to_vec(),
    )
    // Each waiting long-poll holds a worker: keep half the pool free for the rest.
    .with_long_poll_cap((args.workers / 2).max(1)))
}

enum Reply {
    Json(u16, serde_json::Value),
    Raw(u16, String, Vec<u8>),
    Empty(u16),
}
fn err(status: u16, msg: &str) -> Reply {
    Reply::Json(status, json!({ "error": msg }))
}

/// A failure behind the edge (an upstream, the node, Haskoin, the chain
/// reader): logged in full, answered as "`what` is unavailable" — the detail
/// names internal addresses and never reaches a client.
pub(crate) fn unavailable(what: &str, e: impl std::fmt::Display) -> Reply {
    eprintln!("fortis-edge: {what}: {e:#}");
    err(502, &format!("{what} is unavailable"))
}

fn client_ip(req: &Request, trust_xff: bool) -> String {
    let header = |name: &'static str| {
        req.headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    client_ip_from(
        req.remote_addr().map(|a| a.ip()),
        header("CF-Connecting-IP").as_deref(),
        header("X-Forwarded-For").as_deref(),
        trust_xff,
    )
}

/// The client's IP for rate limits and `/register`, as a limiter key.
///
/// Forwarding headers are believed only with `--trust-forwarded-for` *and* a
/// peer on this machine (cloudflared): then Cloudflare's `CF-Connecting-IP`
/// (Cloudflare overwrites it; clients can't set it), else the **last**
/// `X-Forwarded-For` entry — the one the proxy appended. Never the first: the
/// client writes that, and choosing it let anyone pick their own IP and slip
/// every per-IP limit. Values must parse as an IP. An IPv6 client counts as its
/// /64, which one user typically holds.
fn client_ip_from(
    peer: Option<std::net::IpAddr>,
    cf_connecting_ip: Option<&str>,
    x_forwarded_for: Option<&str>,
    trust_xff: bool,
) -> String {
    use std::net::IpAddr;
    let key = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{}/64", std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    };
    let Some(peer) = peer else {
        return "unknown".into();
    };
    if trust_xff && peer.is_loopback() {
        let forwarded = cf_connecting_ip
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
            .or_else(|| x_forwarded_for?.rsplit(',').next()?.trim().parse::<IpAddr>().ok());
        if let Some(ip) = forwarded {
            return key(ip);
        }
    }
    key(peer)
}

/// The install token, from `Authorization: Bearer` only: one in the URL
/// would end up in logs and cache keys.
fn bearer(req: &Request) -> Option<String> {
    let h = req.headers().iter().find(|h| h.field.equiv("Authorization"))?;
    h.value.as_str().strip_prefix("Bearer ").map(str::to_string)
}

fn handle(req: &mut Request, st: &State) -> Reply {
    Metrics::inc(&st.metrics.requests);
    let method = req.method().clone();
    if method == Method::Options {
        return Reply::Empty(204);
    }

    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let path = path.trim_end_matches('/');

    match (&method, path) {
        (Method::Get, "") | (Method::Get, "/") => Reply::Json(
            200,
            json!({
                "name": "fortis-edge",
                "version": env!("CARGO_PKG_VERSION"),
                "chains": {
                    "xbt": st.xbt.is_some(),
                    "btc": st.btc.is_some(),
                },
            }),
        ),
        (Method::Get, "/metrics") => {
            Reply::Raw(200, "text/plain; version=0.0.4".into(), st.metrics.render().into_bytes())
        }
        (Method::Get, "/pricing") => match &st.pricing {
            Some(p) => Reply::Json(200, p.as_json()),
            None => err(404, "no service fee"),
        },
        (Method::Post, "/register") => {
            let ip = client_ip(req, st.trust_forwarded_for);
            if !st.register_limiter.check(&ip) {
                Metrics::inc(&st.metrics.rate_limited);
                return err(429, "too many registrations — try later");
            }
            match token::issue(&st.secret) {
                Ok(t) => {
                    Metrics::inc(&st.metrics.registered);
                    Reply::Json(200, json!({ "token": t }))
                }
                Err(e) => err(500, &e.to_string()),
            }
        }
        (Method::Post, "/crash") => crash_report(req, st),
        (_, p) if p == "/swap" || p.starts_with("/swap/") => swap_route(req, st, &method, p),
        (_, p) if p.starts_with("/xbt/") || p.starts_with("/btc/") => {
            proxy_chain(req, st, &method, p, query)
        }
        _ => err(404, "no such route"),
    }
}

/// `/swap/*`: 404 unless `--swap`; always needs a valid install token, which
/// is also what anti-griefing strikes are counted against.
fn swap_route(req: &mut Request, st: &State, method: &Method, path: &str) -> Reply {
    let Some(sw) = &st.swap else {
        return err(404, "no such route");
    };
    let Some(tok) = bearer(req).filter(|t| token::verify(&st.secret, t)) else {
        Metrics::inc(&st.metrics.unauthorized);
        return err(401, "missing or invalid token — POST /register first");
    };
    if !st.limiter.check(&tok) {
        Metrics::inc(&st.metrics.rate_limited);
        return err(429, "rate limit exceeded");
    }
    swap::route(req, method, path, &tok, sw)
}

/// Append one crash report to the NDJSON log. Body is opaque client JSON, capped
/// at 64 KiB; the line adds a server timestamp and the client IP.
fn crash_report(req: &mut Request, st: &State) -> Reply {
    let Some(path) = st.crash_log.as_ref() else {
        return err(404, "no such route");
    };
    let ip = client_ip(req, st.trust_forwarded_for);
    if !st.crash_limiter.check(&ip) {
        Metrics::inc(&st.metrics.rate_limited);
        return err(429, "slow down");
    }

    let mut body = Vec::new();
    let _ = req.as_reader().take(64 * 1024).read_to_end(&mut body);
    let report: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&body) }));

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = json!({ "ts": ts, "ip": ip, "report": report });

    if let Err(e) = append_capped(path, &line.to_string(), CRASH_LOG_CAP) {
        eprintln!("crash log {}: {e}", path.display());
        return err(500, "could not record");
    }
    Metrics::inc(&st.metrics.crash_reports);
    Reply::Empty(204)
}

/// The crash log's size cap: past it the log moves to `<path>.1` (replacing
/// an older one) and starts afresh, so it never takes much more than twice this.
const CRASH_LOG_CAP: u64 = 10 * 1024 * 1024;

/// Appends `line` and a newline to `path` (created owner-only: lines carry
/// client IPs) in one write, first moving the log to `<path>.1` when the line
/// would take it past `cap`. Serialised in-process, so concurrent reports
/// never interleave or race a rotation.
fn append_capped(path: &std::path::Path, line: &str, cap: u64) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };
    if len > 0 && len + bytes.len() as u64 > cap {
        let mut older = path.as_os_str().to_owned();
        older.push(".1");
        std::fs::rename(path, older)?;
    }
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?.write_all(&bytes)
}

/// Whether `rest` (the path after `/{chain}/`) may go upstream with `method`.
///
/// Refused: any empty, `.` or `..` segment, `%` (no encoded forms), backslashes
/// and control characters — the upstream's URL handling would normalise them,
/// so `POST /btc/./tx` used to reach the node's `/tx` while the service-fee
/// check (which matches `rest == "tx"` exactly) never ran, and `..` could reach
/// other upstream routes. And the only POSTs are `tx` (broadcast, fee-checked)
/// and `scan`.
fn upstream_path_ok(method: &Method, rest: &str) -> Result<(), Reply> {
    let bad_segment = |seg: &str| seg.is_empty() || seg == "." || seg == "..";
    if !rest.is_empty()
        && (rest.split('/').any(bad_segment)
            || rest.chars().any(|c| c == '%' || c == '\\' || c.is_control()))
    {
        return Err(err(400, "bad path"));
    }
    if method == &Method::Post && rest != "tx" && rest != "scan" {
        return Err(err(405, "method not allowed"));
    }
    Ok(())
}

fn proxy_chain(req: &mut Request, st: &State, method: &Method, path: &str, query: &str) -> Reply {
    let (chain, rest) = path[1..].split_once('/').unwrap_or((&path[1..], ""));
    if let Err(r) = upstream_path_ok(method, rest) {
        return r;
    }

    let tok = bearer(req);
    let token_ok = || matches!(&tok, Some(t) if token::verify(&st.secret, t));

    // `GET /{chain}/v1/prices` with a configured price source reads a value a
    // background thread keeps refreshed (see PriceCache) — never a live fetch
    // on the request path, so a slow or wedged price feed can't stall this.
    if method == &Method::Get && rest == "v1/prices" {
        let source = match chain {
            "btc" => st.btc_price.as_ref(),
            "xbt" => st.xbt_price.as_ref(),
            _ => None,
        };
        if let Some(source) = source {
            if st.require_token && !token_ok() {
                Metrics::inc(&st.metrics.unauthorized);
                return err(401, "missing or invalid token — POST /register first");
            }
            return match source.current() {
                Some(usd) => {
                    let rates = st.fx.as_ref().map(fx::FxCache::current).unwrap_or_default();
                    let body = serde_json::to_vec(&fx::prices_body(usd, &rates)).unwrap_or_default();
                    Reply::Raw(200, "application/json".into(), body)
                }
                None => {
                    Metrics::inc(&st.metrics.upstream_errors);
                    err(503, &format!("price source {chain}: not yet available"))
                }
            };
        }
        // no source configured → fall through: /btc/v1/prices proxies to the BTC
        // Esplora upstream; /xbt/v1/prices has no fallback and 404s below.
    }

    // `GET /btc/blocks/tip/height` reads a value TipCache's background thread
    // keeps refreshed — see its doc comment for why this route specifically
    // needed the same treatment as /v1/prices. XBT falls through to its own
    // upstream unchanged; fortis-index has shown no sign of this problem.
    if method == &Method::Get && chain == "btc" && rest == "blocks/tip/height" {
        if let Some(tip) = st.btc_tip.as_ref() {
            if st.require_token && !token_ok() {
                Metrics::inc(&st.metrics.unauthorized);
                return err(401, "missing or invalid token — POST /register first");
            }
            return match tip.current() {
                Some(h) => Reply::Raw(200, "text/plain".into(), h.to_string().into_bytes()),
                None => {
                    Metrics::inc(&st.metrics.upstream_errors);
                    err(503, "btc tip height: not yet available")
                }
            };
        }
    }

    // `GET /btc/debug/history/{address}` — a pure local read of the permanent
    // confirmed-tx store (see `btc_history`'s doc comment), zero upstream
    // calls. Exists purely to answer "is this address's history actually in
    // the database" directly, without that question itself triggering a live
    // fetch the way the normal `/address/{a}/txs` route does (it always tries
    // upstream first, merging on success) — added 2026-09-28 because that
    // distinction genuinely matters and there was no other way to check it
    // without either hitting an upstream or reading the RocksDB files
    // directly, which needs privileges this shell didn't have.
    if method == &Method::Get && chain == "btc" {
        if let Some(addr) = rest.strip_prefix("debug/history/") {
            if st.require_token && !token_ok() {
                Metrics::inc(&st.metrics.unauthorized);
                return err(401, "missing or invalid token — POST /register first");
            }
            let txs = st.btc_history.as_ref().map(|h| h.get(addr)).unwrap_or_default();
            return Reply::Json(200, json!({ "address": addr, "count": txs.len(), "txs": txs }));
        }
    }

    // `POST /btc/prewarm` — body is a JSON array of the wallet's addresses. Pull
    // them all from Haskoin in two calls, reshape into per-address Esplora
    // `/address/{a}/{utxo,txs}` bodies, and prime the cache so the scan that
    // follows is local. On any failure the client falls back to per-address.
    if method == &Method::Post && chain == "btc" && rest == "prewarm" {
        let Some(hs) = &st.btc_haskoin else {
            return err(404, "batch prewarm is not enabled");
        };
        if st.require_token && !token_ok() {
            Metrics::inc(&st.metrics.unauthorized);
            return err(401, "missing or invalid token — POST /register first");
        }
        let mut raw = Vec::new();
        let _ = req.as_reader().take(256 * 1024).read_to_end(&mut raw);
        // 1000, not the old 80: that was sized for a guessed near-next-index
        // window, not a wallet's *real* depth. Found live, 2026-09-15, a
        // genuinely active watch-only import with several hundred used
        // addresses — an 80-address prewarm covers barely a tenth of that, so
        // almost the whole scan still fell through to the slow, one-at-a-time
        // paced path this endpoint exists to avoid. `warm()` chunks its own
        // upstream calls, so this doesn't risk a single oversized URL.
        let addrs: Vec<String> = serde_json::from_slice::<Vec<String>>(&raw)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !a.is_empty() && a.len() < 128)
            .take(1000)
            .collect();
        if addrs.is_empty() {
            return err(400, "expected a non-empty JSON array of addresses");
        }
        return match hs.warm(&addrs) {
            Ok(warmed) => {
                for (a, w) in &warmed {
                    // Merge into the permanent store here too, same as the
                    // per-address GET path (`proxy_chain`'s `/txs` handling
                    // below) — without this, a hosted BTC wallet (the default
                    // has `bulkPrewarm = true`) re-primes this endpoint's own
                    // cache every ~45s while open, so its per-address `/txs`
                    // GETs almost always hit that still-warm cache and never
                    // reach the merge logic at all: the permanent store would
                    // barely ever populate for the most common real-world
                    // case. Replace the cached body with the merged one so a
                    // cache hit later serves the same permanently-backed
                    // answer a live fetch would.
                    // `None` = Haskoin's batch reply may have been cut off for
                    // this address (its limit is set-wide, not per address) —
                    // leave it uncached so the per-address fetch fills it in
                    // correctly rather than serving a truncated list as whole.
                    let txs_body = w.txs.as_ref().map(|raw| {
                        st.btc_history
                            .as_ref()
                            .and_then(|h| serde_json::from_slice::<Vec<serde_json::Value>>(raw).ok().map(|txs| (h, txs)))
                            .map(|(h, txs)| serde_json::to_vec(&h.merge(a, &txs)).unwrap_or_else(|_| raw.clone()))
                            .unwrap_or_else(|| raw.clone())
                    });
                    // `/txs` earns the same long TTL as the regular per-address
                    // path once nothing in it is pending — see `cache::ttl_for`.
                    // `/utxo` keeps the short default (it can genuinely change).
                    for (suffix, body, path) in [
                        ("utxo", w.utxo.as_ref(), format!("/btc/address/{a}/utxo")),
                        ("txs", txs_body.as_ref(), format!("/btc/address/{a}/txs")),
                    ] {
                        let Some(body) = body else { continue };
                        let Some(ttl) = cache::ttl_for(&path, Some(body)) else { continue };
                        st.cache.put(
                            &format!("btc/address/{a}/{suffix}?"),
                            ttl,
                            cache::Cached {
                                status: 200,
                                content_type: "application/json".into(),
                                body: body.clone(),
                            },
                        );
                    }
                }
                Reply::Json(200, json!({ "warmed": warmed.len() }))
            }
            Err(e) => {
                Metrics::inc(&st.metrics.upstream_errors);
                unavailable("the BTC history service", e)
            }
        };
    }

    // `POST /{chain}/scan` — a whole wallet's balance + history in one request.
    if method == &Method::Post && rest == "scan" {
        let authed = token_ok();
        return scan_chain(req, st, chain, tok.as_deref(), authed);
    }

    let upstream = match chain {
        "xbt" => st.xbt.as_ref(),
        "btc" => st.btc.as_ref(),
        _ => None,
    };
    let Some(upstream) = upstream else {
        return err(404, "that chain is not served here");
    };

    if st.require_token && !token_ok() {
        Metrics::inc(&st.metrics.unauthorized);
        return err(401, "missing or invalid token — POST /register first");
    }

    // Cache hits never touch the upstream, so they don't spend rate budget —
    // check the cache before the limiter.
    let cache_key = format!("{chain}/{rest}?{query}");
    let ttl = if method == &Method::Get { cache::ttl_for(path, None) } else { None };
    if ttl.is_some() {
        if let Some(hit) = st.cache.get(&cache_key) {
            Metrics::inc(&st.metrics.cache_hits);
            return Reply::Raw(hit.status, hit.content_type, hit.body);
        }
    }

    let rl_key = tok
        .clone()
        .unwrap_or_else(|| format!("ip:{}", client_ip(req, st.trust_forwarded_for)));
    if !st.limiter.check(&rl_key) {
        Metrics::inc(&st.metrics.rate_limited);
        return err(429, "rate limit exceeded");
    }

    // The only POST that reaches here is `/tx`; a 1 MB (consensus-max) transaction
    // is ~2 MB of hex. Cap the read so a bogus Content-Length can't make a worker
    // allocate unbounded memory.
    let mut body = Vec::new();
    if method == &Method::Post {
        let _ = req.as_reader().take(2 * 1024 * 1024).read_to_end(&mut body);
    }

    // Enforce the service fee on broadcast: the transaction must pay at least
    // the floor to the fee address (the client adds the exact percentage output).
    // A swap HTLC refund/redeem (every input a recorded swap HTLC on this
    // chain, spent with its witness script) is exempt: its commission was paid
    // when the HTLC was funded.
    if method == &Method::Post && rest == "tx" {
        if let Some(p) = &st.pricing {
            let tx_hex = String::from_utf8_lossy(&body);
            let swap_spend = match (&st.swap, swap::parse_chain(chain)) {
                (Some(s), Some(c)) => swap::is_swap_spend(s, c, &tx_hex),
                _ => false,
            };
            if !swap_spend {
                if let Err(msg) = p.check_tx_hex(&tx_hex) {
                    Metrics::inc(&st.metrics.fee_rejected);
                    return err(402, &msg);
                }
            }
        }
    }

    // Broadcast BTC transactions through the local node when configured — it
    // reaches the network directly, relaying what a public Esplora won't (an
    // oversized OP_RETURN). Node-first: on failure, distinguish "node rejected
    // the tx" (return that) from "node unreachable" (fall through to
    // --btc-upstream). Address / history / fee reads always use --btc-upstream.
    if method == &Method::Post && chain == "btc" && rest == "tx" {
        if let Some(rpc) = &st.btc_broadcast {
            let hex = String::from_utf8_lossy(&body).trim().to_string();
            match fortis_node::broadcast(rpc, &hex) {
                Ok(txid) => {
                    return Reply::Raw(200, "text/plain".into(), txid.to_string().into_bytes())
                }
                Err(e) => {
                    // An answer from the node (a rejection) is passed on; the
                    // node unreachable — typed, its text names the node's
                    // address — falls through to the upstream instead.
                    if !e.chain().any(|c| c.is::<fortis_node::Unreachable>()) {
                        Metrics::inc(&st.metrics.upstream_errors);
                        return err(400, &format!("node rejected the transaction: {e:#}"));
                    }
                    Metrics::inc(&st.metrics.upstream_errors);
                    eprintln!(
                        "fortis-edge: BTC node unreachable ({e}); broadcasting via --btc-upstream"
                    );
                    // fall through to the Esplora upstream below
                }
            }
        }
    }

    // A wallet's gap-limit scan is ~40 address lookups back to back; public BTC
    // explorers rate-limit that. Space the address GETs (the cache absorbs the
    // added latency on the next poll). tip / fees / broadcast are untouched.
    if let Some(pacer) = &st.btc_pacer {
        if chain == "btc" && method == &Method::Get && rest.starts_with("address/") {
            pacer.wait();
        }
    }

    // `GET /btc/address/{a}/txs` — if the permanent store already has this
    // address's history, serve it immediately and skip the upstream call
    // entirely, rather than hitting upstream first and falling back to this
    // only on outright failure (the previous order). That order meant a slow
    // upstream (not failing, just slow — confirmed live 2026-09-28: an
    // *unused* address could hang 20+ seconds on one provider before ever
    // reaching this fallback) blocked the response even when the database
    // already had a perfectly good answer sitting there for free.
    // Deliberately only for addresses the store already knows about: an
    // address with no stored history yet (genuinely unused, or never fetched
    // before) still has to ask upstream — there's nothing else to serve.
    // This trades a small amount of freshness (a brand-new tx to an
    // already-used address won't show up until the *next* successful
    // fetch merges it in) for a bounded, database-fast response on this
    // path — acceptable here because this route only matters when the
    // batch `/scan` path (Haskoin, with its own real freshness check via
    // `scan_cache`'s signature comparison) isn't being used at all.
    if chain == "btc" && method == &Method::Get && rest.ends_with("/txs") {
        if let (Some(addr), Some(hist)) =
            (rest.strip_prefix("address/").and_then(|r| r.strip_suffix("/txs")), &st.btc_history)
        {
            let known = hist.get(addr);
            if !known.is_empty() {
                return Reply::Json(200, serde_json::Value::Array(known));
            }
        }
    }

    // No database-first shortcut for `GET /btc/address/{a}/utxo`, unlike
    // `/txs` above — tried this 2026-09-28 and reverted it the same day.
    // `/txs` is safe to serve from the permanent store because a confirmed
    // transaction is a fact that never changes; a UTXO's unspent status is
    // not — it can flip the instant something spends it, and worse, this
    // shortcut ran unconditionally (not just when upstream was struggling),
    // so once an address had *any* stored history it would skip upstream
    // forever, freezing that address's balance at whatever it happened to be
    // the first time this route ever ran for it.

    // When the upstream is flaking (429 after retries, 5xx, transport error), a
    // recently-cached copy keeps a wallet scan from aborting on one bad address.
    let stale = || st.cache.get_stale(&cache_key, std::time::Duration::from_secs(600));

    // 403 alongside 429/5xx: confirmed live, 2026-09-15, that's exactly what
    // Maestro returns once its metered credit budget is exhausted
    // (`{"message":"Credits limit exceeded"}`) — a quota failure is just as
    // much a reason to try the next upstream as a rate limit is, and without
    // this a credits-exhausted primary silently passes that error straight
    // through to every client instead of failing over.
    let bad = |r: &std::result::Result<proxy::UpstreamResponse, anyhow::Error>| match r {
        Ok(resp) => is_bad_upstream(resp.status, method, rest),
        Err(_) => true,
    };
    let mut result = upstream.forward(method, rest, query, &body);
    // `--btc-upstream` failing outright (not just one flaky address, the whole
    // host refusing) is exactly what a single shared egress IP risks at real
    // scale: every wallet behind this edge reads that upstream as one very
    // busy caller, so one explorer's rate limit or outage can take all of
    // them down together. Working through `btc_fallbacks` in order — each an
    // independent provider — means that failure mode costs a slower response
    // instead of a broken one, and keeps trying rather than giving up after
    // one alternate. Only BTC has this — XBT's upstream is our own
    // fortis-index, not a rate-limited public API.
    if chain == "btc" && bad(&result) {
        for fb in &st.btc_fallbacks {
            if !bad(&result) {
                break;
            }
            let fb_result = fb.forward(method, rest, query, &body);
            if !bad(&fb_result) {
                result = fb_result;
            }
        }
    }

    // Confirmed BTC transactions never change, so remember them permanently
    // (see `btc_history`'s doc comment) instead of only via the evictable,
    // 6h-TTL response cache above. Merges in everything ever seen for this
    // address (fuller than any single upstream page can be) on success, and
    // falls back to permanent-only data on a total upstream failure — beyond
    // even the 600s `stale()` grace window `cache.rs` gives every other path.
    // `true` when `result` below is a synthetic 200 built from the permanent
    // store, not a genuine fresh upstream response — see the cache-write
    // guard a few lines down for why that distinction matters.
    let mut served_from_permanent_fallback = false;
    if chain == "btc" && method == &Method::Get && rest.ends_with("/txs") {
        if let (Some(addr), Some(hist)) =
            (rest.strip_prefix("address/").and_then(|r| r.strip_suffix("/txs")), &st.btc_history)
        {
            result = match result {
                Ok(resp) if resp.status == 200 => match serde_json::from_slice::<Vec<serde_json::Value>>(&resp.body) {
                    Ok(txs) => {
                        let merged = hist.merge(addr, &txs);
                        let body = serde_json::to_vec(&merged).unwrap_or(resp.body);
                        Ok(proxy::UpstreamResponse { body, ..resp })
                    }
                    Err(_) => Ok(resp), // unexpected shape — pass through unmerged
                },
                other => {
                    let known = hist.get(addr);
                    if known.is_empty() {
                        other
                    } else {
                        served_from_permanent_fallback = true;
                        let body = serde_json::to_vec(&known).unwrap_or_default();
                        Ok(proxy::UpstreamResponse { status: 200, content_type: "application/json".into(), body })
                    }
                }
            };
        }
    }

    match result {
        Ok(resp) if resp.status == 200 => {
            // Recomputed with the body in hand: a `/txs` response with nothing
            // pending earns the long TTL here, even though the pre-fetch check
            // above (no body yet) only knew the short one.
            //
            // Skip the write entirely when this 200 is the permanent-store
            // fallback: it's whatever confirmed history we had *before*
            // upstream started failing, not a verified-fresh answer. Caching
            // it here would give it a full fresh TTL (up to 6h) — worse than
            // the plain `stale()` grace path below, which serves once and
            // lets the very next request retry upstream, this would instead
            // paper over new activity for hours even after upstream recovers.
            if !served_from_permanent_fallback {
                if let Some(ttl) = if ttl.is_some() { cache::ttl_for(path, Some(&resp.body)) } else { None } {
                    st.cache.put(
                        &cache_key,
                        ttl,
                        cache::Cached {
                            status: 200,
                            content_type: resp.content_type.clone(),
                            body: resp.body.clone(),
                        },
                    );
                }
            }
            Reply::Raw(200, resp.content_type, resp.body)
        }
        Ok(resp) => {
            if ttl.is_some() {
                if let Some(s) = stale() {
                    Metrics::inc(&st.metrics.cache_hits);
                    return Reply::Raw(s.status, s.content_type, s.body);
                }
            }
            Reply::Raw(resp.status, resp.content_type, resp.body)
        }
        Err(e) => {
            if ttl.is_some() {
                if let Some(s) = stale() {
                    Metrics::inc(&st.metrics.cache_hits);
                    return Reply::Raw(s.status, s.content_type, s.body);
                }
            }
            Metrics::inc(&st.metrics.upstream_errors);
            unavailable(&format!("the {chain} service"), e)
        }
    }
}

/// Whether an upstream's answer means "try the next provider" rather than
/// "that's the answer".
///
/// 429 (rate limit), 403 (quota — Maestro once its metered credits ran out)
/// and 5xx are always a reason to move on. So is a 404 on a route every
/// Esplora serves for *any* valid address or for the chain tip: the real
/// thing never 404s there (an unused address is `200 []`), so a 404 means
/// the provider isn't routing the request at all. Found live, 2026-09-19:
/// Maestro started answering every Esplora call with a gateway
/// `404 {"message":"no Route matched with those values"}`, which used to pass
/// straight through as if it were the answer — a funded address read as
/// empty, silently undercounting a BTC wallet by ~0.8 BTC.
fn is_bad_upstream(status: u16, method: &Method, rest: &str) -> bool {
    if status == 429 || status == 403 || (500..=599).contains(&status) {
        return true;
    }
    status == 404
        && method == &Method::Get
        && (rest.starts_with("address/") || rest == "blocks/tip/height" || rest.starts_with("v1/fees"))
}

/// `POST /{chain}/scan` — see `scan.rs`. One rate-limit token for the whole
/// call, however many addresses it names.
fn scan_chain(req: &mut Request, st: &State, chain: &str, tok: Option<&str>, authed: bool) -> Reply {
    let served = match chain {
        "xbt" => st.xbt.is_some(),
        "btc" => st.btc.is_some(),
        _ => false,
    };
    if !served {
        return err(404, "that chain is not served here");
    }
    if st.require_token && !authed {
        Metrics::inc(&st.metrics.unauthorized);
        return err(401, "missing or invalid token — POST /register first");
    }

    // 1000 addresses is ~50 KB; the cap only has to stop a bogus Content-Length.
    let mut raw = Vec::new();
    let _ = req.as_reader().take(256 * 1024).read_to_end(&mut raw);
    let parsed = match scan::ScanRequest::parse(&raw) {
        Ok(p) => p,
        Err(msg) => return err(400, &msg),
    };

    let rl_key = tok.map(str::to_string).unwrap_or_else(|| format!("ip:{}", client_ip(req, st.trust_forwarded_for)));
    if !st.limiter.check(&rl_key) {
        Metrics::inc(&st.metrics.rate_limited);
        return err(429, "rate limit exceeded");
    }

    if chain == "xbt" || st.btc_upstream_scan {
        // fortis-index answers this natively, straight from its address index.
        let up = if chain == "xbt" { st.xbt.as_ref() } else { st.btc.as_ref() };
        let Some(up) = up else { return err(404, "that chain is not served here") };
        return match up.forward(&Method::Post, "scan", "", &parsed.to_body()) {
            Ok(r) => Reply::Raw(r.status, r.content_type, r.body),
            Err(e) => {
                Metrics::inc(&st.metrics.upstream_errors);
                unavailable(&format!("the {chain} service"), e)
            }
        };
    }

    let (Some(hs), Some(tip)) = (st.btc_haskoin.as_ref(), st.btc_tip.as_ref()) else {
        return err(404, "btc scan is not enabled");
    };
    let Some(tip) = tip.current() else {
        Metrics::inc(&st.metrics.upstream_errors);
        return err(503, "btc tip height: not yet available");
    };
    // Best-effort early cache invalidation: forget any address the mempool
    // WS connection has reported activity for since the last scan, so this
    // scan re-fetches it instead of trusting a stale signature match. See
    // mempool_ws's doc comment — this is a pure latency optimization, a
    // no-op whenever it's unconfigured or its connection is down.
    if let Some(mw) = st.btc_mempool_ws.as_ref() {
        if let Some(sc) = st.btc_scan_cache.as_ref() {
            for addr in mw.take_dirty() {
                sc.forget(&addr);
            }
        }
        mw.note(&parsed.addresses);
    }
    match hs.scan(&parsed.addresses, parsed.history, st.btc_history.as_ref(), st.btc_scan_cache.as_ref()) {
        Ok(mut s) => {
            // Haskoin is the only BTC source here that isn't backed by a
            // full node this project runs (it exists purely for its batch-
            // address query — see haskoin.rs's module doc), so its idea of
            // what's still pending isn't guaranteed to match reality. Any
            // entry it calls pending gets one independent check against the
            // real upstream chain before a user ever sees it — see
            // reconcile.rs for why and what was found live.
            if let Some(primary) = st.btc.as_ref() {
                reconcile::drop_dead_pending(primary, &st.btc_fallbacks, &mut s.txs, &mut s.utxos);
            }
            Reply::Json(200, scan::response(tip, s.used, s.utxos, s.txs))
        }
        Err(e) => {
            Metrics::inc(&st.metrics.upstream_errors);
            // Haskoin failing outright doesn't have to mean a hard failure
            // when the permanent store already knows this wallet — the same
            // "answer from what we already have" fallback the per-address
            // routes get above, applied to the whole batch at once instead
            // of one address at a time. Only counts an address as used if
            // the store has *something* for it; nothing here can detect a
            // real address that's used but never yet been fetched, so if
            // the store is empty for every address in the request there's
            // genuinely nothing to serve and this still returns the real
            // 502 below rather than a wrong "empty wallet" answer.
            //
            // Deliberately serves `txs` but never `utxos`/balance from this
            // path. A UTXO can only be correctly derived from *complete*
            // transaction history (an output looks unspent unless some known
            // tx spends it) — found live, 2026-09-28: if even one spending
            // transaction was never successfully fetched and merged in (very
            // plausible during exactly the kind of upstream instability that
            // makes this fallback trigger at all), a genuinely spent output
            // reads as still-unspent and the balance comes out *higher* than
            // reality. Understating a wallet's history (some old txs missing
            // from the list) is a safe, honest kind of stale; overstating its
            // spendable balance is not, so utxos stays empty here rather than
            // risk it — an empty balance is an obviously-degraded state a
            // user won't act on by mistake, a wrong-but-plausible-looking one
            // might be.
            let mut used = Vec::new();
            let mut txs = Vec::new();
            if let Some(hist) = st.btc_history.as_ref() {
                for addr in &parsed.addresses {
                    let known = hist.get(addr);
                    if known.is_empty() {
                        continue;
                    }
                    txs.extend(known);
                    used.push(addr.clone());
                }
            }
            if used.is_empty() {
                return unavailable("the BTC history service", e);
            }
            // Same dedup + "pending first, then confirmed newest-first, cut
            // to the requested depth" ordering the healthy path uses — this
            // fallback previously just concatenated each address's list in
            // request order, which is not a coherent history to show at all.
            let txs = haskoin::merge_history(txs, parsed.history);
            eprintln!(
                "fortis-edge: haskoin scan failed ({e:#}); serving {} known address(es) from the permanent store (history only, no balance)",
                used.len()
            );
            Reply::Json(200, scan::response_ex(tip, used, Vec::new(), txs, true))
        }
    }
}

fn respond(req: Request, reply: Reply, allow_origin: &str) -> std::io::Result<()> {
    let (status, ctype, data): (u16, String, Vec<u8>) = match reply {
        Reply::Empty(s) => (s, "text/plain".into(), Vec::new()),
        Reply::Json(s, v) => (s, "application/json".into(), serde_json::to_vec(&v).unwrap_or_default()),
        Reply::Raw(s, ct, b) => (s, ct, b),
    };
    let mut resp = Response::from_data(data).with_status_code(status);
    for (k, v) in [
        ("Access-Control-Allow-Origin", allow_origin),
        ("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS"),
        ("Access-Control-Allow-Headers", "authorization, content-type, x-swap-secret"),
        ("Content-Type", ctype.as_str()),
    ] {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            resp.add_header(h);
        }
    }
    req.respond(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- chain proxy paths: no dot segments, no encoding, POST only to tx/scan ----

    #[test]
    fn ordinary_upstream_paths_are_allowed() {
        for rest in ["", "tx", "blocks/tip/height", "v1/fees/recommended",
                     "address/bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh/utxo",
                     "tx/0af166a14c972ad1f2e0cb06953e7e63d67ce786d2142f2bca8823248f79990f/outspend/0",
                     "txout/0af166a14c972ad1f2e0cb06953e7e63d67ce786d2142f2bca8823248f79990f/1"] {
            assert!(upstream_path_ok(&Method::Get, rest).is_ok(), "{rest}");
        }
        assert!(upstream_path_ok(&Method::Post, "tx").is_ok());
        assert!(upstream_path_ok(&Method::Post, "scan").is_ok());
    }

    /// `POST /btc/./tx` used to skip the service-fee check (it only ran for
    /// `rest == "tx"`) while the upstream normalised it back to `/tx`.
    #[test]
    fn dot_segments_and_encodings_that_could_dodge_the_fee_check_are_refused() {
        for rest in ["./tx", "a/../tx", "..", ".", "%2e/tx", "%2E%2E/tx", "tx/", "a//b", "a\\b", "tx\n", "tx%00"] {
            assert!(upstream_path_ok(&Method::Post, rest).is_err(), "POST {rest:?}");
            assert!(upstream_path_ok(&Method::Get, rest).is_err(), "GET {rest:?}");
        }
    }

    #[test]
    fn the_only_posts_forwarded_are_tx_and_scan() {
        for rest in ["address/x/utxo", "txs", "tx/abc", "blocks/tip/height"] {
            assert!(upstream_path_ok(&Method::Post, rest).is_err(), "{rest}");
        }
    }

    // ---- client IP behind Cloudflare (rate limits, /register) ----

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn behind_the_tunnel_the_ip_is_cloudflares_not_the_clients_own_claim() {
        let local = Some(ip("127.0.0.1"));
        // An attacker's X-Forwarded-For comes first; Cloudflare appends the real one.
        assert_eq!(client_ip_from(local, Some("203.0.113.9"), Some("6.6.6.6, 203.0.113.9"), true), "203.0.113.9");
        // Without CF-Connecting-IP, the last (proxy-appended) entry — never the first.
        assert_eq!(client_ip_from(local, None, Some("6.6.6.6, 203.0.113.9"), true), "203.0.113.9");
        // A malformed header is ignored, not trusted.
        assert_eq!(client_ip_from(local, Some("not-an-ip"), Some("6.6.6.6, junk"), true), "127.0.0.1");
    }

    #[test]
    fn forwarding_headers_count_only_from_this_machine() {
        let remote = Some(ip("198.51.100.7"));
        assert_eq!(client_ip_from(remote, Some("203.0.113.9"), Some("203.0.113.9"), true), "198.51.100.7");
        assert_eq!(client_ip_from(Some(ip("127.0.0.1")), Some("203.0.113.9"), None, false), "127.0.0.1");
        assert_eq!(client_ip_from(None, None, None, true), "unknown");
    }

    // ---- crash log: bounded on disk ----

    fn crash_dir() -> (crate::test_tmp::TempDir, PathBuf) {
        let d = crate::test_tmp::TempDir::new("crash-log-test");
        std::fs::create_dir_all(&*d).unwrap();
        let path = d.join("crashes.ndjson");
        (d, path)
    }

    #[test]
    fn crash_lines_are_appended_whole() {
        let (_d, path) = crash_dir();
        append_capped(&path, "{\"a\":1}", 1_000).unwrap();
        append_capped(&path, "{\"b\":2}", 1_000).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
    }

    /// Past the cap the log moves to `<path>.1` (replacing an older one) and
    /// starts afresh: at most two files of about `cap` bytes each.
    #[test]
    fn the_crash_log_rotates_at_its_cap() {
        let (_d, path) = crash_dir();
        let old = path.with_extension("ndjson.1");
        let line = "x".repeat(99); // 100 bytes with the newline
        for _ in 0..10 {
            append_capped(&path, &line, 1_000).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1_000);
        assert!(!old.exists());
        append_capped(&path, "first", 1_000).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        assert_eq!(std::fs::metadata(&old).unwrap().len(), 1_000);
        // 6 + 9 × 100 bytes fit; the tenth line would pass the cap.
        for _ in 0..10 {
            append_capped(&path, &line, 1_000).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{line}\n"));
        let older = std::fs::read_to_string(&old).unwrap();
        assert!(older.starts_with("first\n"), "the older .1 is replaced");
        assert_eq!(older.len(), 906);
    }

    #[test]
    fn concurrent_crash_lines_never_interleave() {
        let (_d, path) = crash_dir();
        let threads: Vec<_> = (0..8u8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let line = ((b'a' + t) as char).to_string().repeat(5_000);
                    for _ in 0..20 {
                        append_capped(&path, &line, 10_000_000).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 160);
        for l in text.lines() {
            assert_eq!(l.len(), 5_000);
            assert!(l.chars().all(|c| c == l.chars().next().unwrap()), "interleaved line");
        }
    }

    /// Long-polls may hold half the workers: with 4, two swaps' polls (or a
    /// few slow upstream calls) left every other request waiting.
    #[test]
    fn the_default_worker_pool_has_room_for_long_polls_and_slow_calls() {
        let args = Args::parse_from(["fortis-edge"]);
        assert_eq!(args.workers, 32);
    }

    #[test]
    fn an_ipv6_client_counts_as_its_slash_64() {
        let local = Some(ip("::1"));
        assert_eq!(client_ip_from(local, Some("2001:db8:1:2:3:4:5:6"), None, true), "2001:db8:1:2::/64");
        assert_eq!(client_ip_from(local, Some("2001:db8:1:2:ffff::1"), None, true), "2001:db8:1:2::/64");
    }

    #[test]
    fn build_swap_refuses_send_floor_above_swap_floor() {
        let dir = std::env::temp_dir().join(format!("fortis-edge-main-test-{}", std::process::id()));
        let addr = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
        let run = |floor: &str| {
            let args = Args::parse_from([
                "fortis-edge", "--swap", "--service-fee-address", addr, "--network", "regtest",
                "--service-fee-floor-sat", floor, "--swap-db", dir.to_str().unwrap(),
            ]);
            build_swap(&args, b"key").map(|_| ())
        };
        let e = run("500").unwrap_err().to_string();
        assert!(e.contains("--service-fee-floor-sat"), "{e}");
        assert!(run("400").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_swap_refuses_a_fee_address_whose_dust_limit_is_above_the_swap_floor() {
        let dir = std::env::temp_dir().join(format!("fortis-edge-main-test-dust-{}", std::process::id()));
        let run = |addr: &str| {
            let args = Args::parse_from([
                "fortis-edge", "--swap", "--service-fee-address", addr, "--network", "regtest",
                "--swap-db", dir.to_str().unwrap(),
            ]);
            build_swap(&args, b"key").map(|_| ())
        };
        let p2pkh = bitcoin::Address::p2pkh(
            <bitcoin::PubkeyHash as bitcoin::hashes::Hash>::from_byte_array([7; 20]),
            bitcoin::Network::Regtest,
        )
        .to_string();
        let e = run(&p2pkh).unwrap_err().to_string();
        assert_eq!(
            e,
            "--service-fee-address must be a SegWit address with --swap: its dust limit (546 sat) is above the 400 sat minimum commission"
        );
        assert!(run("bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gateway_404_on_an_address_or_tip_route_means_try_the_next_provider() {
        let get = Method::Get;
        // Maestro's "no Route matched" 404 must fail over, not pass through as an empty answer.
        assert!(is_bad_upstream(404, &get, "address/bc1qabc/utxo"));
        assert!(is_bad_upstream(404, &get, "address/bc1qabc/txs"));
        assert!(is_bad_upstream(404, &get, "blocks/tip/height"));
        // the existing failure classes still fail over
        for s in [429, 403, 500, 502, 503] {
            assert!(is_bad_upstream(s, &get, "address/bc1qabc/txs"), "{s}");
        }
        // a genuine "not found" (unknown tx) is an answer, and 200 obviously is
        assert!(!is_bad_upstream(404, &get, "tx/deadbeef"));
        assert!(!is_bad_upstream(200, &get, "address/bc1qabc/utxo"));
        assert!(!is_bad_upstream(404, &Method::Post, "address/bc1qabc/utxo"));
    }

}
