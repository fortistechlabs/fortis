//! fortisd — a small local HTTP gateway between the fortis web wallet and a
//! Bitcoin Knots / BLAKE2b node. It holds **no keys**: the browser signs; this
//! serves chain data (UTXOs, fees, history, status) and broadcasts finished
//! transactions.
//!
//! Every `/v1/*` route needs `Authorization: Bearer <token>` (printed on startup,
//! stored at `<home>/fortisd.token`). CORS is open by default (`--allow-origin`)
//! since the token is the real guard.

mod handlers;
mod state;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use fortis_node::Rpc;
use state::{default_bitcoin_datadir, default_home, load_or_create_token, Settings, State};

#[derive(Parser)]
#[command(name = "fortisd", version, about = "fortis chain-gateway daemon (holds no keys)")]
struct Args {
    /// Node data directory (holds `.cookie`). Default: the platform Bitcoin dir.
    #[arg(long)]
    datadir: Option<PathBuf>,
    /// Node RPC URL. Default: 127.0.0.1:8332 (:18443 for regtest).
    #[arg(long)]
    rpc_url: Option<String>,
    /// Node network: mainnet | regtest.
    #[arg(long, default_value = "mainnet")]
    network: String,
    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8088")]
    bind: String,
    /// State + token directory. Default: %APPDATA%\fortisd or ~/.fortisd.
    #[arg(long)]
    home: Option<PathBuf>,
    /// Explicit RPC cookie file (overrides datadir/.cookie).
    #[arg(long)]
    cookie_file: Option<String>,
    /// CORS `Access-Control-Allow-Origin` value.
    #[arg(long, default_value = "*")]
    allow_origin: String,
    /// Forward `/esplora/*` to this Esplora API (e.g. https://mempool.guide/api),
    /// adding CORS. Lets the web wallet use a public explorer that lacks CORS.
    /// A node is optional in this mode.
    #[arg(long)]
    esplora_proxy: Option<String>,
    /// A local Bitcoin Core node's RPC URL (e.g. http://127.0.0.1:8532). When
    /// set, the BTC side's broadcast (`POST /esplora/tx`) and fee estimation
    /// (`GET /esplora/v1/fees/recommended`) run against this node instead of the
    /// `--esplora-proxy` upstream; address/history reads still go upstream.
    #[arg(long)]
    btc_rpc_url: Option<String>,
    /// Data directory of the `--btc-rpc-url` node (for its `.cookie`).
    #[arg(long)]
    btc_datadir: Option<PathBuf>,
    /// Explicit cookie file for the `--btc-rpc-url` node (overrides `--btc-datadir`).
    #[arg(long)]
    btc_cookie_file: Option<String>,
    /// Print the API token and exit.
    #[arg(long)]
    print_token: bool,
    /// Charge a service fee on sends through this gateway (address to receive
    /// it). Omit to run free — the normal choice for self-hosting.
    #[arg(long)]
    service_fee_address: Option<String>,
    /// Service fee, in basis points (1/100 of a percent) of the amount sent.
    #[arg(long, default_value_t = 25)]
    service_fee_bps: u32,
    /// Minimum service fee, satoshis.
    #[arg(long, default_value_t = 200)]
    service_fee_floor_sat: u64,
    /// Maximum service fee, satoshis (0 = uncapped).
    #[arg(long, default_value_t = 5_000)]
    service_fee_cap_sat: u64,
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
    let home = args.home.clone().unwrap_or_else(default_home);
    let (token, fresh) = load_or_create_token(&home)?;
    if args.print_token {
        println!("{token}");
        return Ok(());
    }

    let rpc_url = args.rpc_url.clone().unwrap_or_else(|| {
        if args.network.starts_with("regtest") {
            "http://127.0.0.1:18443".into()
        } else {
            "http://127.0.0.1:8332".into()
        }
    });
    let datadir = args
        .datadir
        .clone()
        .unwrap_or_else(default_bitcoin_datadir)
        .to_string_lossy()
        .into_owned();

    let pricing = args.service_fee_address.clone().map(|address| state::Pricing {
        address,
        bps: args.service_fee_bps,
        floor_sat: args.service_fee_floor_sat,
        cap_sat: args.service_fee_cap_sat,
    });
    let settings = Settings {
        datadir,
        rpc_url,
        network: args.network.clone(),
        cookie_file: args.cookie_file.clone(),
        allow_origin: args.allow_origin.clone(),
        esplora_proxy: args.esplora_proxy.clone(),
        home: home.clone(),
        pricing,
    };
    let proxy_only = settings.esplora_proxy.is_some();

    let rpc = match &settings.cookie_file {
        Some(cf) => Rpc::new(&settings.rpc_url, std::fs::read_to_string(cf)?.trim()),
        None => match Rpc::new_cookie(&settings.rpc_url, &settings.datadir, &settings.network) {
            Ok(r) => r,
            Err(e) if proxy_only => {
                eprintln!("note: no node ({e}); serving the Esplora proxy only");
                Rpc::new(&settings.rpc_url, "x:x")
            }
            Err(e) => return Err(e),
        },
    };
    // Reach the node unless we're proxy-only.
    let node_line = match fortis_node::chain_status(&rpc) {
        Ok(cs) => format!("{}  ({}, chain {})", settings.rpc_url, cs.subversion, cs.chain),
        Err(e) if proxy_only => format!("(no node — {e})"),
        Err(e) => return Err(e),
    };

    // Optional local Bitcoin Core node for the BTC broadcast + fee-estimation path.
    let btc_rpc = match &args.btc_rpc_url {
        None => {
            if args.btc_datadir.is_some() || args.btc_cookie_file.is_some() {
                return Err(anyhow!("--btc-datadir / --btc-cookie-file need --btc-rpc-url"));
            }
            None
        }
        Some(url) => {
            let rpc = match &args.btc_cookie_file {
                Some(cf) => Rpc::new(
                    url,
                    std::fs::read_to_string(cf).with_context(|| format!("reading {cf}"))?.trim(),
                ),
                None => {
                    let dd = args
                        .btc_datadir
                        .as_ref()
                        .ok_or_else(|| anyhow!("--btc-rpc-url needs --btc-datadir or --btc-cookie-file"))?;
                    Rpc::new_cookie(url, &dd.to_string_lossy(), "mainnet")?
                }
            };
            match fortis_node::chain_status(&rpc) {
                Ok(cs) => eprintln!("btc node →  {url}  ({}, chain {})", cs.subversion, cs.chain),
                Err(e) => return Err(anyhow!("cannot reach the BTC node at {url}: {e}")),
            }
            Some(rpc)
        }
    };

    let mut st = State::load(&home);
    let server = Server::http(&args.bind).map_err(|e| anyhow!("cannot bind {}: {e}", args.bind))?;

    eprintln!("fortisd  →  {node_line}");
    if let Some(u) = &settings.esplora_proxy {
        eprintln!("           esplora proxy: /esplora/*  →  {u}");
        if btc_rpc.is_some() {
            eprintln!("           btc broadcast + fees served from the local node");
        }
    }
    if let Some(p) = &settings.pricing {
        eprintln!(
            "           service fee: {}bps of amount sent, min {} sat, {} → {}",
            p.bps,
            p.floor_sat,
            if p.cap_sat > 0 { format!("max {} sat", p.cap_sat) } else { "uncapped".into() },
            p.address
        );
    }
    if let Some(c) = &st.connected {
        eprintln!("           serving {} / {}  (wallet {})", c.chain, c.network, c.watch_wallet);
    }
    eprintln!("listening on  http://{}", args.bind);
    eprintln!();
    eprintln!("  connect the web app with:");
    if let Some(_u) = &settings.esplora_proxy {
        eprintln!("     explorer URL   http://{}/esplora", args.bind);
    }
    eprintln!("     gateway URL    http://{}", args.bind);
    eprintln!("     token          {token}{}", if fresh { "   (newly generated)" } else { "" });
    eprintln!();

    let nodes = Nodes { chain: &rpc, btc: btc_rpc.as_ref() };
    let http = esplora_agent();
    for mut req in server.incoming_requests() {
        let raw_body = read_body(&mut req);
        let reply = handle(&req, &nodes, &mut st, &settings, &token, &raw_body, &http);
        let _ = respond(req, reply, &settings.allow_origin);
    }
    Ok(())
}

/// The node RPC connections a request may need: the primary node (Knots for XBT,
/// or the single node in a one-chain deployment) and an optional local Bitcoin
/// Core for the BTC broadcast + fee path.
struct Nodes<'a> {
    chain: &'a Rpc,
    btc: Option<&'a Rpc>,
}

/// What a handler produced. `Text` is a verbatim body (Esplora proxy passthrough).
enum Reply {
    Empty(u16),
    Json(u16, Value),
    Text(u16, String),
}
fn ok(v: Value) -> Reply {
    Reply::Json(200, v)
}
fn err(status: u16, msg: impl std::fmt::Display) -> Reply {
    Reply::Json(status, json!({ "error": msg.to_string() }))
}

fn read_body(req: &mut Request) -> String {
    if req.method() == &Method::Post {
        // Bodies here are small JSON commands / a tx hex. Cap the read at 2 MiB so
        // an unauthenticated request with a bogus Content-Length can't exhaust
        // memory (the body is read before the bearer-token check).
        let mut buf = String::new();
        let _ = req.as_reader().take(2 * 1024 * 1024).read_to_string(&mut buf);
        buf
    } else {
        String::new()
    }
}

/// Length-then-content compare with no early exit on a content mismatch, so a
/// network attacker can't time their way to the bearer token byte by byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn handle(
    req: &Request,
    nodes: &Nodes,
    st: &mut State,
    settings: &Settings,
    token: &str,
    raw_body: &str,
    http: &ureq::Agent,
) -> Reply {
    let method = req.method().clone();
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));

    if method == Method::Options {
        return Reply::Empty(204);
    }
    if path == "/" && method == Method::Get {
        return ok(json!({ "name": "fortisd", "version": env!("CARGO_PKG_VERSION") }));
    }

    // Esplora path: /esplora/<rest>. Public data only, so no token required (bind
    // to localhost, or trust your network). A local BTC node, when configured,
    // serves broadcast + fee estimation; everything else forwards to the upstream
    // explorer with CORS added.
    if let Some(rest) = path.strip_prefix("/esplora/") {
        if let Some(btc) = nodes.btc {
            match (&method, rest) {
                (Method::Post, "tx") => return esplora_broadcast(btc, raw_body),
                (Method::Get, "v1/fees/recommended") => return esplora_fees(btc),
                _ => {}
            }
        }
        return match &settings.esplora_proxy {
            Some(upstream) => proxy_esplora(http, &method, upstream, rest, query, raw_body),
            None => err(404, "no such route"),
        };
    }

    let authorized = req.headers().iter().any(|h| {
        h.field.equiv("Authorization")
            && h.value
                .as_str()
                .strip_prefix("Bearer ")
                .is_some_and(|t| ct_eq(t.as_bytes(), token.as_bytes()))
    });
    if !authorized {
        return err(401, "missing or invalid bearer token");
    }

    let body: Value = if raw_body.trim().is_empty() {
        Value::Null
    } else {
        match serde_json::from_str(raw_body) {
            Ok(v) => v,
            Err(e) => return err(400, format!("bad JSON body: {e}")),
        }
    };

    let rpc = nodes.chain;
    let result: Result<Value> = match (&method, path) {
        (Method::Get, "/v1/status") => handlers::status(rpc, st, settings.pricing.as_ref()),
        (Method::Post, "/v1/connect") => serde_json::from_value(body)
            .map_err(Into::into)
            .and_then(|r| handlers::connect(rpc, st, &settings.home, r)),
        (Method::Get, "/v1/balances") => handlers::balances(rpc, st),
        (Method::Get, "/v1/utxos") => handlers::utxos(rpc, st, q_u32(query, "min_conf").unwrap_or(1)),
        (Method::Get, "/v1/feerate") => {
            handlers::feerate(rpc, q_u32(query, "conf_target").unwrap_or(6) as u16)
        }
        (Method::Get, "/v1/history") => handlers::history(rpc, st, q_u32(query, "count").unwrap_or(50)),
        (Method::Post, "/v1/broadcast") => serde_json::from_value(body).map_err(Into::into).and_then(|r| {
            handlers::broadcast(rpc, r, settings.pricing.as_ref(), &settings.network)
        }),
        _ => return err(404, "no such route"),
    };

    match result {
        Ok(v) => ok(v),
        Err(e) => err(400, format!("{e:#}")),
    }
}

fn esplora_agent() -> ureq::Agent {
    // ureq's `native-tls` feature needs the connector wired in explicitly.
    let mut b = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(30));
    if let Ok(tls) = native_tls::TlsConnector::new() {
        b = b.tls_connector(std::sync::Arc::new(tls));
    }
    b.build()
}

/// Esplora `POST /tx` served locally: the body is raw tx hex, the response is the
/// txid as plain text (matching a real Esplora), or a plain-text error.
fn esplora_broadcast(btc: &Rpc, raw_body: &str) -> Reply {
    match fortis_node::broadcast(btc, raw_body.trim()) {
        Ok(txid) => Reply::Text(200, txid.to_string()),
        Err(e) => Reply::Text(400, format!("{e:#}")),
    }
}

/// Esplora `GET /v1/fees/recommended` served from the local node's
/// `estimatesmartfee` at a few confirmation targets.
fn esplora_fees(btc: &Rpc) -> Reply {
    let at = |t: u16| fortis_node::estimate_feerate(btc, t).unwrap_or(1);
    ok(recommended_fees(at(1), at(3), at(6), at(144)))
}

/// Shape the four buckets like mempool.space, clamped so they never invert
/// (`fastest >= halfHour >= hour >= economy`).
fn recommended_fees(fastest: u64, half_hour: u64, hour: u64, economy: u64) -> Value {
    let half_hour = half_hour.min(fastest);
    let hour = hour.min(half_hour);
    let economy = economy.min(hour);
    json!({
        "fastestFee": fastest,
        "halfHourFee": half_hour,
        "hourFee": hour,
        "economyFee": economy,
        "minimumFee": 1u64,
    })
}

fn proxy_esplora(agent: &ureq::Agent, method: &Method, upstream: &str, rest: &str, query: &str, body: &str) -> Reply {
    let base = upstream.trim_end_matches('/');
    let target = if query.is_empty() {
        format!("{base}/{rest}")
    } else {
        format!("{base}/{rest}?{query}")
    };
    let resp = match method {
        Method::Get => agent.get(&target).call(),
        Method::Post => agent.post(&target).set("Content-Type", "text/plain").send_string(body),
        _ => return err(405, "method not allowed"),
    };
    match resp {
        Ok(r) => Reply::Text(r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => Reply::Text(code, r.into_string().unwrap_or_default()),
        Err(e) => err(502, format!("upstream explorer: {e}")),
    }
}

fn respond(req: Request, reply: Reply, allow_origin: &str) -> std::io::Result<()> {
    let (status, ctype, data): (u16, &str, Vec<u8>) = match reply {
        Reply::Empty(s) => (s, "application/json", Vec::new()),
        Reply::Json(s, v) => (s, "application/json", serde_json::to_vec(&v).unwrap_or_default()),
        Reply::Text(s, t) => (s, "text/plain", t.into_bytes()),
    };
    let mut resp = Response::from_data(data).with_status_code(status);
    for (name, value) in [
        ("Access-Control-Allow-Origin", allow_origin),
        ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
        ("Access-Control-Allow-Headers", "authorization, content-type"),
        ("Content-Type", ctype),
    ] {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            resp.add_header(h);
        }
    }
    req.respond(resp)
}

fn q_u32(query: &str, key: &str) -> Option<u32> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::recommended_fees;

    #[test]
    fn fee_buckets_never_invert() {
        // estimatesmartfee can report a *higher* rate for a longer target;
        // clamp so the app's fast/normal/slow picks stay ordered.
        let f = recommended_fees(5, 9, 2, 7);
        assert_eq!(f["fastestFee"], 5);
        assert_eq!(f["halfHourFee"], 5);
        assert_eq!(f["hourFee"], 2);
        assert_eq!(f["economyFee"], 2);
        assert_eq!(f["minimumFee"], 1);
    }

    #[test]
    fn fee_buckets_pass_through_when_already_ordered() {
        let f = recommended_fees(10, 8, 5, 3);
        assert_eq!(f["fastestFee"], 10);
        assert_eq!(f["halfHourFee"], 8);
        assert_eq!(f["hourFee"], 5);
        assert_eq!(f["economyFee"], 3);
    }
}
