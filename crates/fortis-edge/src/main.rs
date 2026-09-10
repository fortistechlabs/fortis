//! fortis-edge — the public front for the fortis wallet backends.
//!
//! It sits in front of a `fortis-index` instance (XBT) and an Esplora upstream
//! (BTC — a public explorer, or a `fortisd --esplora-proxy`) and adds what a
//! backend exposed to many wallets needs: per-install tokens, per-key rate
//! limiting, short-TTL response caching, locked-down CORS, and `/metrics`.
//! Stateless (HMAC tokens, in-memory limiter/cache) so instances scale out.
//!
//! TLS is expected from a reverse proxy (Caddy / nginx) in front.

mod cache;
mod haskoin;
mod limit;
mod metrics;
mod price;
mod pricing;
mod proxy;
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
    /// Sustained requests per minute, per token (or per IP if untokened). One
    /// wallet sync fans out to ~1 + 2·(addresses within the gap limit) requests,
    /// and repeats on every refresh, so this is generous per install.
    #[arg(long, default_value_t = 600)]
    rate_per_min: u32,
    /// Rate-limit bucket capacity (burst). Must cover a whole gap-limit scan
    /// (tip + utxo + txs per address) landing at once.
    #[arg(long, default_value_t = 300)]
    rate_burst: u32,
    /// `POST /register` calls allowed per hour, per client IP.
    #[arg(long, default_value_t = 10)]
    register_per_hour: u32,
    /// Max cached responses.
    #[arg(long, default_value_t = 4096)]
    cache_entries: usize,
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
    btc_broadcast: Option<fortis_node::Rpc>,
    btc_price: Option<price::PriceSource>,
    xbt_price: Option<price::PriceSource>,
    btc_haskoin: Option<haskoin::HaskoinStore>,
    btc_pacer: Option<Pacer>,
    limiter: RateLimiter,
    register_limiter: RateLimiter,
    crash_limiter: RateLimiter,
    crash_log: Option<PathBuf>,
    pricing: Option<pricing::Pricing>,
    cache: Cache,
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

    let state = Arc::new(State {
        secret,
        require_token: args.require_token,
        trust_forwarded_for: args.trust_forwarded_for,
        allow_origin: args.allow_origin.clone(),
        xbt: args.xbt_upstream.as_deref().map(Upstream::new),
        btc: args.btc_upstream.as_deref().map(Upstream::new),
        btc_broadcast,
        btc_price: args.btc_price_url.as_deref().map(price::PriceSource::new),
        xbt_price: xbt_price_url.as_deref().map(price::PriceSource::new),
        btc_haskoin: (!args.btc_haskoin_url.trim().is_empty())
            .then(|| haskoin::HaskoinStore::new(&args.btc_haskoin_url, args.btc_haskoin_key.clone())),
        btc_pacer: (args.btc_upstream_rate > 0.0).then(|| Pacer::new(args.btc_upstream_rate)),
        limiter: RateLimiter::new(args.rate_per_min, args.rate_burst),
        register_limiter: RateLimiter::new(args.register_per_hour, args.register_per_hour.max(1)),
        // crashes are rare per device; this just caps a crash-looping client or abuse
        crash_limiter: RateLimiter::new(2, 8),
        crash_log: args.crash_log.clone(),
        pricing: fee,
        cache: Cache::new(args.cache_entries),
        metrics: Metrics::default(),
    });

    let server = Arc::new(
        Server::http(&args.bind).map_err(|e| anyhow!("cannot bind {}: {e}", args.bind))?,
    );

    eprintln!("fortis-edge listening on  http://{}", args.bind);
    eprintln!("  xbt upstream   {}", args.xbt_upstream.as_deref().unwrap_or("(none)"));
    eprintln!("  btc upstream   {}", args.btc_upstream.as_deref().unwrap_or("(none)"));
    eprintln!("  btc broadcast  {}", args.btc_rpc_url.as_deref().unwrap_or("(via btc upstream)"));
    eprintln!("  btc price      {}", args.btc_price_url.as_deref().unwrap_or("(via btc upstream)"));
    eprintln!("  xbt price    {}", xbt_price_url.as_deref().unwrap_or("(none)"));
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

    // `GET /{chain}/v1/prices` with a configured price source is normalised, not
    // proxied: fetch the source and return `{ "USD": <spot> }`, cached 60 s.
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
            let key = format!("{chain}/v1/prices");
            if let Some(hit) = st.cache.get(&key) {
                Metrics::inc(&st.metrics.cache_hits);
                return Reply::Raw(hit.status, hit.content_type, hit.body);
            }
            return match source.fetch_usd() {
                Ok(usd) => {
                    let body = serde_json::to_vec(&json!({ "USD": usd })).unwrap_or_default();
                    st.cache.put(
                        &key,
                        std::time::Duration::from_secs(60),
                        cache::Cached {
                            status: 200,
                            content_type: "application/json".into(),
                            body: body.clone(),
                        },
                    );
                    Reply::Raw(200, "application/json".into(), body)
                }
                Err(e) => {
                    Metrics::inc(&st.metrics.upstream_errors);
                    err(502, &format!("price source {chain}: {e}"))
                }
            };
        }
        // no source configured → fall through: /btc/v1/prices proxies to the BTC
        // Esplora upstream; /xbt/v1/prices has no fallback and 404s below.
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
        let _ = req.as_reader().take(64 * 1024).read_to_end(&mut raw);
        let addrs: Vec<String> = serde_json::from_slice::<Vec<String>>(&raw)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !a.is_empty() && a.len() < 128)
            .take(80)
            .collect();
        if addrs.is_empty() {
            return err(400, "expected a non-empty JSON array of addresses");
        }
        return match hs.warm(&addrs) {
            Ok(warmed) => {
                let ttl = std::time::Duration::from_secs(60);
                for (a, w) in &warmed {
                    for (suffix, body) in [("utxo", &w.utxo), ("txs", &w.txs)] {
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
    let ttl = if method == &Method::Get { cache::ttl_for(path) } else { None };
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

    match upstream.forward(method, rest, query, &body) {
        Ok(resp) if resp.status == 200 => {
            if let Some(ttl) = ttl {
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
