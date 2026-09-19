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
mod limit;
mod metrics;
mod price;
mod pricing;
mod proxy;
mod scan;
mod token;

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
    /// Cap on `/btc/address/*` lookups per second sent to `--btc-upstream`.
    /// A wallet scan fans out ~40 of them at once and public explorers
    /// (mempool.space, blockstream.info) 429 the burst; the edge queues them
    /// instead. `0` disables. Only affects BTC — a `fortis-index` has no limit.
    #[arg(long, default_value_t = 5.0)]
    btc_upstream_rate: f64,
    /// Batch source for BTC address data — a Haskoin Store base URL. Enables
    /// `POST /btc/prewarm`, which fetches a wallet's whole address set in two
    /// calls and pre-fills the cache, so the per-address scan is served locally
    /// instead of fanned out to `--btc-upstream`. Default is the Haskoin
    /// project's instance; blockchain.com's is capped at ~1000/day and unusable.
    /// Empty disables (the client falls back to the paced per-address path).
    #[arg(long, default_value = "https://api.haskoin.com/btc")]
    btc_haskoin_url: String,
    /// Optional `X-API-Key` header for `--btc-haskoin-url` (api.haskoin.com needs
    /// none).
    #[arg(long)]
    btc_haskoin_key: Option<String>,
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
    /// Max cached responses (the fast in-memory layer; the RocksDB directory
    /// backing it has no such cap).
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
    /// HTTP worker threads.
    #[arg(long, default_value_t = 4)]
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
    btc_haskoin: Option<haskoin::HaskoinStore>,
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
    metrics: Metrics,
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
        // though it's the real per-request chain's primary.
        btc_tip: {
            let chain = build_btc_tip_chain();
            (!chain.is_empty()).then(|| proxy::TipCache::spawn(chain, std::time::Duration::from_secs(20)))
        },
        btc_broadcast,
        btc_price: args
            .btc_price_url
            .as_deref()
            .map(|u| price::PriceCache::spawn(price::PriceSource::new(u), std::time::Duration::from_secs(60))),
        xbt_price: xbt_price_url
            .as_deref()
            .map(|u| price::PriceCache::spawn(price::PriceSource::new(u), std::time::Duration::from_secs(60))),
        btc_haskoin: (!args.btc_haskoin_url.trim().is_empty())
            .then(|| haskoin::HaskoinStore::new(&args.btc_haskoin_url, args.btc_haskoin_key.clone())),
        btc_pacer: (args.btc_upstream_rate > 0.0).then(|| Pacer::new(args.btc_upstream_rate)),
        limiter: RateLimiter::new(args.rate_per_min, args.rate_burst),
        register_limiter: RateLimiter::new(args.register_per_hour, args.register_per_hour.max(1)),
        // crashes are rare per device; this just caps a crash-looping client or abuse
        crash_limiter: RateLimiter::new(2, 8),
        crash_log: args.crash_log.clone(),
        pricing: fee,
        cache: Cache::new(args.cache_entries, &cache_dir),
        btc_history: BtcHistory::new(&btc_history_dir),
        metrics: Metrics::default(),
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
        "  btc pacing     {}",
        if args.btc_upstream_rate > 0.0 {
            format!("{}/s on /address/*", args.btc_upstream_rate)
        } else {
            "(off)".into()
        },
    );
    eprintln!(
        "  btc batch      {}",
        if args.btc_haskoin_url.trim().is_empty() {
            "(off)".into()
        } else {
            format!("{} (/btc/prewarm)", args.btc_haskoin_url)
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

enum Reply {
    Json(u16, serde_json::Value),
    Raw(u16, String, Vec<u8>),
    Empty(u16),
}
fn err(status: u16, msg: &str) -> Reply {
    Reply::Json(status, json!({ "error": msg }))
}

fn client_ip(req: &Request, trust_xff: bool) -> String {
    if trust_xff {
        if let Some(xff) = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("X-Forwarded-For"))
        {
            if let Some(first) = xff.value.as_str().split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    req.remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn bearer(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().iter().find(|h| h.field.equiv("Authorization")) {
        if let Some(t) = h.value.as_str().strip_prefix("Bearer ") {
            return Some(t.to_string());
        }
    }
    // fallback: ?token= in the query
    let url = req.url();
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "token")
        .map(|(_, v)| v.to_string())
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
        (_, p) if p.starts_with("/xbt/") || p.starts_with("/btc/") => {
            proxy_chain(req, st, &method, p, query)
        }
        _ => err(404, "no such route"),
    }
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

    let ok = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"))
        .is_ok();
    if !ok {
        return err(500, "could not record");
    }
    Metrics::inc(&st.metrics.crash_reports);
    Reply::Empty(204)
}

fn proxy_chain(req: &mut Request, st: &State, method: &Method, path: &str, query: &str) -> Reply {
    let (chain, rest) = path[1..].split_once('/').unwrap_or((&path[1..], ""));

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
                    let body = serde_json::to_vec(&json!({ "USD": usd })).unwrap_or_default();
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
                err(502, &format!("haskoin: {e}"))
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
    if method == &Method::Post && rest == "tx" {
        if let Some(p) = &st.pricing {
            if let Err(msg) = p.check_tx_hex(&String::from_utf8_lossy(&body)) {
                Metrics::inc(&st.metrics.fee_rejected);
                return err(402, &msg);
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
                    if rpc.call("getblockcount", json!([])).is_ok() {
                        // node is up — it genuinely rejected the transaction
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
            err(502, &format!("upstream {chain}: {e}"))
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

    if chain == "xbt" {
        // fortis-index answers this natively, straight from its address index.
        let Some(up) = st.xbt.as_ref() else { return err(404, "that chain is not served here") };
        return match up.forward(&Method::Post, "scan", "", &parsed.to_body()) {
            Ok(r) => Reply::Raw(r.status, r.content_type, r.body),
            Err(e) => {
                Metrics::inc(&st.metrics.upstream_errors);
                err(502, &format!("upstream xbt: {e}"))
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
    match hs.scan(&parsed.addresses, parsed.history) {
        Ok(s) => Reply::Json(200, scan::response(tip, s.used, s.utxos, s.txs)),
        Err(e) => {
            Metrics::inc(&st.metrics.upstream_errors);
            err(502, &format!("haskoin: {e}"))
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
        ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
        ("Access-Control-Allow-Headers", "authorization, content-type"),
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
