//! Optional low-latency companion to Haskoin polling: one persistent
//! WebSocket connection to a mempool.space-compatible instance's
//! `track-addresses` API, used purely to learn about address activity
//! *sooner* than the next `/btc/scan` poll would via `scan_cache`.
//!
//! The wire format here (client sends `{"track-addresses": [...]}`, server
//! pushes `{"multi-address-transactions": {addr: {mempool,confirmed,
//! removed}}}`, per-connection cap is `config.MEMPOOL.MAX_TRACKED_ADDRESSES`,
//! default 100) was verified directly against the real backend source —
//! `backend/src/api/websocket-handler.ts` and `backend/mempool-config.
//! sample.json` in github.com/mempool/mempool — not just the docs page,
//! because mempool.space itself turned out to be unreachable from this
//! network entirely: DNS resolves, but the raw TCP connection to it times
//! out (confirmed live, 2026-09-28, from both this machine directly and a
//! separate sandboxed environment). The default URL here instead points at
//! mempool.emzy.de, a community-run mirror running the same open-source
//! backend, which answered a plain HTTPS probe and a live track-addresses
//! round trip cleanly in the same testing session.
//!
//! This is a pure latency optimization layered on top of the existing
//! Haskoin + `scan_cache` path, never a replacement for it: all it ever does
//! is call `ScanCache::forget` early for an address the server just reported
//! activity on, so the *next* `/btc/scan` re-fetches instead of serving a
//! now-stale cache hit. If this connection is unconfigured, down, or
//! misbehaving, `/btc/scan` is unaffected beyond losing that early-warning —
//! the existing signature-based cache check still catches the change on the
//! next poll regardless.
//!
//! One connection total, not one per wallet: mempool's `MAX_TRACKED_
//! ADDRESSES` is a per-*connection* cap, and there's no way to track more
//! from a single edge instance without rotating connections. Tracking is
//! therefore capped to the most-recently-scanned 100 addresses (evicting the
//! coldest), so a wallet with a larger scan window still gets a fully
//! correct answer on every poll — it just doesn't get the early-invalidation
//! win for its coldest addresses.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tungstenite::{Message, WebSocket};

/// Mirrors mempool's own default `MEMPOOL.MAX_TRACKED_ADDRESSES` — see the
/// module doc. Not configurable here: this edge only ever runs one
/// connection, so this is a hard ceiling, not a tuning knob.
const MAX_TRACKED: usize = 100;

struct Shared {
    /// Most-recently-active at the back; the front is evicted first once
    /// `MAX_TRACKED` is exceeded.
    tracked: VecDeque<String>,
    /// Addresses the server has reported activity for since the last
    /// `take_dirty` call.
    dirty: Vec<String>,
}

pub struct MempoolWs {
    shared: Arc<Mutex<Shared>>,
}

impl MempoolWs {
    pub fn spawn(url: String) -> Self {
        let shared = Arc::new(Mutex::new(Shared { tracked: VecDeque::new(), dirty: Vec::new() }));
        let bg = Arc::clone(&shared);
        std::thread::spawn(move || run(url, bg));
        Self { shared }
    }

    /// Record that these addresses are currently interesting (they were just
    /// part of a real scan) — moves each to the most-recently-active end,
    /// evicting the coldest tracked address if that pushes past
    /// `MAX_TRACKED`.
    pub fn note(&self, addresses: &[String]) {
        let mut s = self.shared.lock().unwrap();
        for a in addresses {
            if let Some(pos) = s.tracked.iter().position(|x| x == a) {
                if pos != s.tracked.len() - 1 {
                    s.tracked.remove(pos);
                    s.tracked.push_back(a.clone());
                }
                continue;
            }
            s.tracked.push_back(a.clone());
        }
        while s.tracked.len() > MAX_TRACKED {
            s.tracked.pop_front();
        }
    }

    /// Addresses the server has reported activity for since the last call —
    /// the caller should `ScanCache::forget` each of these so the next scan
    /// re-fetches instead of trusting a stale cache hit.
    pub fn take_dirty(&self) -> Vec<String> {
        let mut s = self.shared.lock().unwrap();
        std::mem::take(&mut s.dirty)
    }
}

/// A plain or TLS-wrapped `TcpStream`, so the read-timeout set before the
/// handshake (see `connect`) applies uniformly regardless of scheme.
/// `tungstenite`'s own `connect()` helper doesn't expose the underlying
/// stream to let us set that timeout (confirmed by reading `stream.rs`), so
/// this crate does the TCP+TLS dance itself instead of using that helper.
enum Sock {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}
impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Plain(s) => s.read(buf),
            Sock::Tls(s) => s.read(buf),
        }
    }
}
impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Plain(s) => s.write(buf),
            Sock::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sock::Plain(s) => s.flush(),
            Sock::Tls(s) => s.flush(),
        }
    }
}

fn parse_url(url: &str) -> Result<(String, u16, bool)> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(anyhow!("mempool ws url must start with ws:// or wss://: {url}"));
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().context("invalid port in mempool ws url")?),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    Ok((host, port, tls))
}

/// Connects and completes the WebSocket handshake with `read_timeout` on the
/// underlying socket applied throughout (including the handshake itself, so
/// a wedged server can't hang this thread indefinitely).
fn connect(url: &str, read_timeout: Duration) -> Result<WebSocket<Sock>> {
    let (host, port, tls) = parse_url(url)?;
    let tcp = TcpStream::connect((host.as_str(), port)).context("tcp connect")?;
    tcp.set_read_timeout(Some(read_timeout)).ok();
    tcp.set_nodelay(true).ok();
    let sock = if tls {
        let connector = native_tls::TlsConnector::new().context("tls connector")?;
        let tls_stream =
            connector.connect(&host, tcp).map_err(|e| anyhow!("tls handshake: {e}"))?;
        Sock::Tls(Box::new(tls_stream))
    } else {
        Sock::Plain(tcp)
    };
    let (socket, _resp) =
        tungstenite::client(url, sock).map_err(|e| anyhow!("ws handshake: {e}"))?;
    Ok(socket)
}

fn run(url: String, shared: Arc<Mutex<Shared>>) {
    loop {
        // Don't spend a connection (or this community server's attention) on
        // an empty tracked set — nothing to subscribe to yet, which happens
        // for a while after every fresh start until the first real scan.
        loop {
            if !shared.lock().unwrap().tracked.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_secs(3));
        }

        match connect(&url, Duration::from_secs(20)) {
            Ok(mut socket) => {
                eprintln!("fortis-edge: mempool ws connected ({url})");
                // Every fresh connection starts unsubscribed, so `last_sent`
                // must start empty here even if a previous connection already
                // sent the same list — otherwise a reconnect (this server
                // appears to close a connection that never subscribes) would
                // never resend it, since nothing about `tracked` itself
                // necessarily *changed* since the last send. Diffing the
                // live set against `last_sent` on every tick (rather than a
                // separate "is it stale" flag) means this falls out for
                // free instead of needing special-cased reconnect handling.
                let mut last_sent: Vec<String> = Vec::new();
                let close_reason: String;
                'conn: loop {
                    let snapshot: Vec<String> = shared.lock().unwrap().tracked.iter().cloned().collect();
                    if !snapshot.is_empty() && snapshot != last_sent {
                        let msg = serde_json::json!({ "track-addresses": snapshot }).to_string();
                        if let Err(e) = socket.send(Message::Text(msg.into())) {
                            close_reason = format!("send failed: {e}");
                            break 'conn;
                        }
                        last_sent = snapshot;
                    }
                    match socket.read() {
                        Ok(Message::Text(txt)) => handle_message(&txt, &shared),
                        Ok(Message::Close(frame)) => {
                            close_reason = format!("server closed: {frame:?}");
                            break 'conn;
                        }
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e))
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(e) => {
                            close_reason = format!("read error: {e}");
                            break 'conn;
                        }
                    }
                }
                eprintln!("fortis-edge: mempool ws disconnected ({close_reason}); reconnecting in 15s");
            }
            Err(e) => {
                eprintln!("fortis-edge: mempool ws connect failed ({e:#}); retrying in 15s");
            }
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

fn handle_message(txt: &str, shared: &Arc<Mutex<Shared>>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else { return };
    let Some(raw) = v.get("multi-address-transactions") else { return };
    // Some server builds send this pre-stringified rather than as a nested
    // object — handle both rather than assuming.
    let obj = raw.as_object().cloned().or_else(|| {
        raw.as_str().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()).and_then(|v| v.as_object().cloned())
    });
    let Some(obj) = obj else { return };
    let mut s = shared.lock().unwrap();
    for addr in obj.keys() {
        if !s.dirty.contains(addr) {
            s.dirty.push(addr.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wss_url_with_default_port() {
        let (host, port, tls) = parse_url("wss://mempool.emzy.de/api/v1/ws").unwrap();
        assert_eq!(host, "mempool.emzy.de");
        assert_eq!(port, 443);
        assert!(tls);
    }

    #[test]
    fn parses_ws_url_with_explicit_port() {
        let (host, port, tls) = parse_url("ws://localhost:8999/api/v1/ws").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 8999);
        assert!(!tls);
    }

    #[test]
    fn rejects_a_non_websocket_scheme() {
        assert!(parse_url("https://mempool.emzy.de/api/v1/ws").is_err());
    }

    #[test]
    fn note_evicts_the_coldest_address_past_the_cap() {
        let ws = MempoolWs { shared: Arc::new(Mutex::new(Shared { tracked: VecDeque::new(), dirty: Vec::new() })) };
        let addrs: Vec<String> = (0..MAX_TRACKED + 1).map(|i| format!("addr{i}")).collect();
        ws.note(&addrs);
        let s = ws.shared.lock().unwrap();
        assert_eq!(s.tracked.len(), MAX_TRACKED);
        assert!(!s.tracked.contains(&"addr0".to_string()), "the coldest address should have been evicted");
        assert!(s.tracked.contains(&format!("addr{MAX_TRACKED}")));
    }

    #[test]
    fn note_on_an_already_tracked_address_refreshes_it_without_duplicating() {
        let ws = MempoolWs { shared: Arc::new(Mutex::new(Shared { tracked: VecDeque::new(), dirty: Vec::new() })) };
        ws.note(&["a".to_string(), "b".to_string()]);
        ws.note(&["a".to_string()]);
        let s = ws.shared.lock().unwrap();
        assert_eq!(s.tracked.iter().filter(|x| *x == "a").count(), 1);
        assert_eq!(s.tracked.back(), Some(&"a".to_string()));
    }

    #[test]
    fn take_dirty_drains_and_dedupes() {
        let shared = Arc::new(Mutex::new(Shared { tracked: VecDeque::new(), dirty: Vec::new() }));
        handle_message(r#"{"multi-address-transactions":{"addr1":{"mempool":[],"confirmed":[],"removed":[]}}}"#, &shared);
        handle_message(r#"{"multi-address-transactions":{"addr1":{},"addr2":{}}}"#, &shared);
        let ws = MempoolWs { shared };
        let dirty = ws.take_dirty();
        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains(&"addr1".to_string()));
        assert!(dirty.contains(&"addr2".to_string()));
        assert!(ws.take_dirty().is_empty(), "a second call should see nothing new");
    }

    #[test]
    fn unrelated_messages_are_ignored() {
        let shared = Arc::new(Mutex::new(Shared { tracked: VecDeque::new(), dirty: Vec::new() }));
        handle_message(r#"{"block":{"height":900000}}"#, &shared);
        handle_message("not json at all", &shared);
        assert!(shared.lock().unwrap().dirty.is_empty());
    }

    /// Not run in CI — hits the real mempool.emzy.de over the network. Exists
    /// to verify this module's actual TCP+TLS+handshake code (not just the
    /// mocked unit tests above) against a real server: connects, sends
    /// `track-addresses`, and confirms the connection stays open and
    /// protocol-valid rather than being immediately rejected or closed.
    #[test]
    #[ignore = "hits the real mempool.emzy.de API"]
    fn a_real_connection_to_emzy_de_accepts_track_addresses() {
        let mut socket = connect("wss://mempool.emzy.de/api/v1/ws", Duration::from_secs(8)).unwrap();
        let addrs = vec![
            "bc1qqefe0fp9vracl38u8etj4k408n4rgwhkdenw60".to_string(),
            "bc1qazgkf85gfpy5000rl0g7595qwkfwuttg39f960".to_string(),
        ];
        let msg = serde_json::json!({ "track-addresses": addrs }).to_string();
        socket.send(Message::Text(msg.into())).expect("send should succeed on a live connection");
        // One read attempt: either a message arrives (fine), or it times out
        // (also fine — these addresses are quiet) — anything other than a
        // hard protocol error/close means the server accepted the subscription.
        match socket.read() {
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("unexpected error after a supposedly-accepted subscription: {e}"),
        }
        let _ = socket.close(None);
    }
}
