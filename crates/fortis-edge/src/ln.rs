//! Lightning (phase 1): `GET /ln/info` and the `/ln/vss/*` reverse proxy.
//!
//! - [`LnInfoSource`]: the LSP's address, fees and limits plus the kill
//!   switch (`enabled` / `message`), from the operator's `--ln-info-file`.
//!   Validated on load; re-read when the file changes (polled every
//!   [`INFO_POLL`]); an invalid new file is ignored and logged, and the last
//!   good one keeps being served.
//! - [`VssProxy`]: forwards the four VSS API calls to the local vss-server.
//!   The edge adds no state and no VSS credentials: vss-server runs with its
//!   no-op authorizer and a namespace is the seed-derived 256-bit store id
//!   inside the (client-encrypted) request body. The edge's part is abuse
//!   control — install token, a rate-limit bucket of its own, body caps, an
//!   exact path allowlist, upstream timeouts — and relaying the upstream's
//!   status and body byte for byte (clients depend on 409 `ConflictError`).
//!   Nothing here logs a body, a token or a store id.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tiny_http::Method;

use crate::{err, Reply};

/// Largest `--ln-info-file` accepted.
pub const INFO_FILE_MAX: u64 = 64 * 1024;
/// How often the info file's mtime (and size, inode) is checked.
pub const INFO_POLL: Duration = Duration::from_secs(1);
/// The networks an info file may name (as fortis-lsp `/info` spells them).
pub const NETWORKS: [&str; 4] = ["bitcoin", "testnet", "signet", "regtest"];
/// Longest operator `message` shown to users.
const MESSAGE_MAX: usize = 1000;
/// Longest LSPS2 token.
const TOKEN_MAX: usize = 512;

/// The `/ln/info` document. Unknown fields in the file are ignored (and not
/// served); every field here is required except `lsps2_token` and `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LnInfo {
    /// The LSP's node id: a compressed public key, 66 hex digits.
    pub node_id: String,
    /// Bare hostname or IP the LSP listens on (no scheme, path or port).
    pub host: String,
    pub port: u64,
    /// LSPS2 token, when the LSP requires one.
    #[serde(default)]
    pub lsps2_token: Option<String>,
    pub fee: Fee,
    /// The EFFECTIVE payment bounds the LSP enforces (fortis-lsp `/info`'s
    /// `min/max_payment_size_msat`, in sat — see the README section).
    pub min_payment_sat: u64,
    pub max_payment_sat: u64,
    pub channel_min_sat: u64,
    pub channel_max_sat: u64,
    /// Kill switch: `false` tells clients not to start new Lightning use.
    /// VSS keeps working regardless (exits need backups).
    pub enabled: bool,
    /// Shown to users, e.g. why Lightning is paused.
    #[serde(default)]
    pub message: Option<String>,
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fee {
    /// Channel-open fee, parts per million of the payment.
    pub ppm: u64,
    /// Channel-open fee floor, sat.
    pub min_sat: u64,
}

/// Parses and validates an info file's contents. Error messages name the
/// field at fault but never echo the token.
pub fn parse_info(bytes: &[u8]) -> Result<LnInfo> {
    let mut info: LnInfo = serde_json::from_slice(bytes).map_err(|e| anyhow!("not a valid ln info document: {e}"))?;
    let is_hex = info.node_id.len() == 66 && info.node_id.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex || bitcoin::secp256k1::PublicKey::from_str(&info.node_id).is_err() {
        bail!("node_id must be a compressed public key (66 hex digits)");
    }
    info.node_id.make_ascii_lowercase();
    let host_ok = !info.host.is_empty()
        && info.host.len() <= 253
        && info.host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        && !info.host.contains("://");
    if !host_ok {
        bail!("host must be a bare hostname or IP address (no scheme, path, port or whitespace)");
    }
    if !(1..=65535).contains(&info.port) {
        bail!("port must be 1..65535, not {}", info.port);
    }
    if let Some(t) = &info.lsps2_token {
        if t.is_empty() || t.len() > TOKEN_MAX || t.chars().any(char::is_control) {
            bail!("lsps2_token must be 1..{TOKEN_MAX} printable characters (or absent)");
        }
    }
    if info.fee.ppm > 100_000 {
        bail!("fee.ppm ({}) is above 100000 (10%)", info.fee.ppm);
    }
    if info.min_payment_sat > info.max_payment_sat {
        bail!("min_payment_sat ({}) > max_payment_sat ({})", info.min_payment_sat, info.max_payment_sat);
    }
    if info.channel_min_sat > info.channel_max_sat {
        bail!("channel_min_sat ({}) > channel_max_sat ({})", info.channel_min_sat, info.channel_max_sat);
    }
    if let Some(m) = &info.message {
        if m.chars().count() > MESSAGE_MAX {
            bail!("message is longer than {MESSAGE_MAX} characters");
        }
    }
    if !NETWORKS.contains(&info.network.as_str()) {
        bail!("network must be one of {}, not {:?}", NETWORKS.join(", "), info.network);
    }
    Ok(info)
}

/// What identifies one version of the file: a rewrite in place changes the
/// mtime (and usually the size), an editor's write-and-rename the inode.
type Stamp = Option<(Option<SystemTime>, u64, u64)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).ok().map(|m| (m.modified().ok(), m.len(), m.ino()))
}

fn read_capped(path: &Path) -> Result<LnInfo> {
    let f = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    f.take(INFO_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() as u64 > INFO_FILE_MAX {
        bail!("{} is larger than {} KiB", path.display(), INFO_FILE_MAX / 1024);
    }
    parse_info(&bytes)
}

/// The served `/ln/info`: the last good version of `--ln-info-file`.
pub struct LnInfoSource {
    path: PathBuf,
    current: RwLock<(Arc<Vec<u8>>, LnInfo)>,
    seen: Mutex<Stamp>,
}

impl LnInfoSource {
    /// Loads and validates `path`; an error here should stop the edge.
    pub fn open(path: &Path) -> Result<LnInfoSource> {
        let seen = stamp(path);
        let info = read_capped(path)?;
        let body = Arc::new(serde_json::to_vec(&info)?);
        Ok(LnInfoSource { path: path.to_path_buf(), current: RwLock::new((body, info)), seen: Mutex::new(seen) })
    }

    /// The JSON body to serve.
    pub fn body(&self) -> Arc<Vec<u8>> {
        Arc::clone(&self.current.read().unwrap_or_else(|e| e.into_inner()).0)
    }

    pub fn info(&self) -> LnInfo {
        self.current.read().unwrap_or_else(|e| e.into_inner()).1.clone()
    }

    /// One poll: re-reads the file if it changed. `Some(true)` = a new
    /// version is served, `Some(false)` = the change was rejected (logged; the
    /// last good one stays), `None` = unchanged.
    pub fn poll_once(&self) -> Option<bool> {
        let now = stamp(&self.path);
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if *seen == now {
                return None;
            }
            // Taken before the read: a write that lands during it changes the
            // stamp again and is picked up next poll.
            *seen = now;
        }
        if now.is_none() {
            eprintln!("fortis-edge: ln info: {} is gone; keeping the last good one", self.path.display());
            return Some(false);
        }
        match read_capped(&self.path) {
            Ok(info) => {
                let body = match serde_json::to_vec(&info) {
                    Ok(b) => Arc::new(b),
                    Err(e) => {
                        eprintln!("fortis-edge: ln info: cannot encode {}: {e}; keeping the last good one", self.path.display());
                        return Some(false);
                    }
                };
                eprintln!(
                    "fortis-edge: ln info: reloaded {} (enabled={}, network {})",
                    self.path.display(),
                    info.enabled,
                    info.network
                );
                *self.current.write().unwrap_or_else(|e| e.into_inner()) = (body, info);
                Some(true)
            }
            Err(e) => {
                eprintln!(
                    "fortis-edge: ln info: ignoring invalid {}: {e:#}; keeping the last good one",
                    self.path.display()
                );
                Some(false)
            }
        }
    }

    /// Polls the file every `every` on a background thread, for good.
    pub fn watch(self: &Arc<Self>, every: Duration) {
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("ln-info-watch".into())
            .spawn(move || loop {
                std::thread::sleep(every);
                me.poll_once();
            })
            .expect("spawn ln-info watcher");
    }
}

/// The VSS API calls the proxy forwards: vss-server's `/vss/<op>` routes.
pub const VSS_OPS: [&str; 4] = ["getObject", "putObjects", "deleteObject", "listKeyVersions"];
/// `--vss-max-body` default: 4 MiB.
pub const VSS_MAX_BODY_DEFAULT: usize = 4 * 1024 * 1024;
/// Largest upstream answer relayed; more is a 502.
pub const VSS_MAX_RESPONSE: u64 = 8 * 1024 * 1024;
pub const VSS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const VSS_READ_TIMEOUT_SECS: u64 = 30;

/// Which VSS call `rest` (the path after `/ln/vss/`, untouched) names. Only
/// an exact match of one of [`VSS_OPS`] passes, so dot segments, double
/// slashes and percent-encodings can never reach the upstream: 404 for any
/// other path, 405 for an allowed path with a method other than POST.
pub fn vss_op(method: &Method, rest: &str) -> Result<&'static str, Reply> {
    let Some(op) = VSS_OPS.iter().copied().find(|op| *op == rest) else {
        return Err(err(404, "no such route"));
    };
    if method != &Method::Post {
        return Err(err(405, "method not allowed"));
    }
    Ok(op)
}

/// An upstream answer, relayed as is.
pub struct Relay {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum VssFail {
    /// Connect or read timeout: 504.
    Timeout,
    /// The answer is over [`VSS_MAX_RESPONSE`]: 502.
    TooLarge,
    /// Anything else (refused, reset, bad response): 502. The detail is for
    /// the log only — it names the internal upstream.
    Unavailable(String),
}

pub struct VssProxy {
    base: String,
    agent: ureq::Agent,
    pub max_body: usize,
    quota: Option<ByteQuota>,
}

/// `--vss-quota-bytes-per-hour` default: 256 MiB of writes per install.
pub const VSS_QUOTA_DEFAULT: u64 = 256 * 1024 * 1024;
const QUOTA_WINDOW: Duration = Duration::from_secs(3600);
/// Keys a quota holds before it drops the expired windows.
const QUOTA_MAX_KEYS: usize = 10_000;

/// Bytes of `putObjects` body one install may send per fixed window. The
/// proxy keeps no per-store state, so this bounds how fast any one install
/// can fill the VSS disk; the rate limiter alone allows `max_body` per
/// request. In memory, per instance.
pub struct ByteQuota {
    limit: u64,
    window: Duration,
    max_keys: usize,
    used: Mutex<QuotaMap>,
}

#[derive(Default)]
struct QuotaMap {
    entries: std::collections::HashMap<String, (std::time::Instant, u64)>,
    last_sweep: Option<std::time::Instant>,
}

impl ByteQuota {
    pub fn new(limit: u64, window: Duration) -> ByteQuota {
        ByteQuota {
            limit,
            window,
            max_keys: QUOTA_MAX_KEYS,
            used: Mutex::new(QuotaMap::default()),
        }
    }

    #[cfg(test)]
    fn with_max_keys(mut self, max_keys: usize) -> ByteQuota {
        self.max_keys = max_keys;
        self
    }

    /// Takes `bytes` from `key`'s window; `false` (nothing taken) when that
    /// would pass the limit.
    pub fn charge(&self, key: &str, bytes: u64) -> bool {
        self.charge_at(key, bytes, std::time::Instant::now())
    }

    fn charge_at(&self, key: &str, bytes: u64, now: std::time::Instant) -> bool {
        if bytes > self.limit {
            return false;
        }
        let mut guard = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((start, used)) = guard.entries.get_mut(key) {
            if now.saturating_duration_since(*start) >= self.window {
                *start = now;
                *used = 0;
            }
            return match used.checked_add(bytes) {
                Some(total) if total <= self.limit => {
                    *used = total;
                    true
                }
                _ => false,
            };
        }
        if guard.entries.len() >= self.max_keys {
            let sweep_due = guard
                .last_sweep
                .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(10));
            if sweep_due {
                let window = self.window;
                guard
                    .entries
                    .retain(|_, (start, _)| now.saturating_duration_since(*start) < window);
                guard.last_sweep = Some(now);
            }
            if guard.entries.len() >= self.max_keys {
                return false;
            }
        }
        guard.entries.insert(key.to_string(), (now, bytes));
        true
    }
}

impl VssProxy {
    pub fn new(base: &str, max_body: usize, read_timeout: Duration) -> Result<VssProxy> {
        let base = base.trim_end_matches('/');
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            bail!("--vss-upstream must be an http:// or https:// URL");
        }
        if max_body == 0 {
            bail!("--vss-max-body must be at least 1");
        }
        let mut b = ureq::AgentBuilder::new()
            .timeout_connect(VSS_CONNECT_TIMEOUT)
            .timeout_read(read_timeout)
            .timeout_write(read_timeout)
            // A redirect is relayed, never followed: a POST turned into a GET
            // elsewhere is not what the client asked for.
            .redirects(0);
        if let Ok(tls) = native_tls::TlsConnector::new() {
            b = b.tls_connector(Arc::new(tls));
        }
        Ok(VssProxy { base: base.to_string(), agent: b.build(), max_body, quota: None })
    }

    /// Limits each install token to `bytes_per_hour` of `putObjects` bodies
    /// (0 = no limit).
    pub fn with_quota(mut self, bytes_per_hour: u64) -> VssProxy {
        self.quota = (bytes_per_hour > 0).then(|| ByteQuota::new(bytes_per_hour, QUOTA_WINDOW));
        self
    }

    /// Whether `tok` may write `bytes` now. Only `putObjects` adds storage;
    /// every other call, and a proxy without a quota, is always allowed.
    pub fn allow_write(&self, op: &str, tok: &str, bytes: usize) -> bool {
        op != "putObjects" || self.quota.as_ref().is_none_or(|q| q.charge(tok, bytes as u64))
    }

    /// POSTs `body` to `<base>/<op>` with only `Content-Type` set (ureq adds
    /// Host, Content-Length, User-Agent, Accept; nothing from the client).
    pub fn forward(&self, op: &str, content_type: &str, body: &[u8]) -> Result<Relay, VssFail> {
        let url = format!("{}/{}", self.base, op);
        let resp = match self.agent.post(&url).set("Content-Type", content_type).send_bytes(body) {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) if is_timeout(&e) => return Err(VssFail::Timeout),
            Err(e) => return Err(VssFail::Unavailable(e.to_string())),
        };
        let status = resp.status();
        let content_type = resp.header("Content-Type").unwrap_or("application/octet-stream").to_string();
        let mut out = Vec::new();
        match resp.into_reader().take(VSS_MAX_RESPONSE + 1).read_to_end(&mut out) {
            Ok(_) if out.len() as u64 > VSS_MAX_RESPONSE => Err(VssFail::TooLarge),
            Ok(_) => Ok(Relay { status, content_type, body: out }),
            Err(e) if is_timeout(&e) => Err(VssFail::Timeout),
            Err(e) => Err(VssFail::Unavailable(format!("reading the answer: {e}"))),
        }
    }
}

/// Whether `e` (or anything in its source chain) is an I/O timeout.
fn is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(x) = cur {
        if let Some(io) = x.downcast_ref::<std::io::Error>() {
            if matches!(io.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
                return true;
            }
        }
        cur = x.source();
    }
    false
}

/// The request's `Content-Type`, passed upstream only when it is short,
/// printable ASCII; otherwise VSS's own `application/octet-stream`.
pub fn safe_content_type(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| (0x20..0x7f).contains(&b)) => v.to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp::TempDir;
    use serde_json::{json, Value};

    const NODE_ID: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn good() -> Value {
        json!({
            "node_id": NODE_ID, "host": "lsp.example.org", "port": 9735, "lsps2_token": "t",
            "fee": { "ppm": 10000, "min_sat": 2000 },
            "min_payment_sat": 51000, "max_payment_sat": 250000,
            "channel_min_sat": 100000, "channel_max_sat": 1000000,
            "enabled": true, "message": null, "network": "bitcoin", "extra": [1, 2],
        })
    }

    fn with(f: impl FnOnce(&mut Value)) -> Result<LnInfo> {
        let mut v = good();
        f(&mut v);
        parse_info(&serde_json::to_vec(&v).unwrap())
    }

    #[test]
    fn quota_limits_writes_per_window_and_key() {
        use std::time::Instant;
        let q = ByteQuota::new(100, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(q.charge_at("a", 60, t0));
        assert!(!q.charge_at("a", 41, t0), "over the limit takes nothing");
        assert!(q.charge_at("a", 40, t0), "the refused charge left the budget alone");
        assert!(q.charge_at("b", 100, t0), "another key has its own budget");
        assert!(!q.charge_at("a", 1, t0 + Duration::from_secs(59)));
        assert!(q.charge_at("a", 100, t0 + Duration::from_secs(60)), "a new window");
        assert!(!q.charge_at("a", u64::MAX, t0 + Duration::from_secs(61)), "no overflow");
    }

    #[test]
    fn quota_caps_active_keys_and_sweeps_expired_ones() {
        use std::time::Instant;
        let q = ByteQuota::new(100, Duration::from_secs(60)).with_max_keys(2);
        let t0 = Instant::now();
        assert!(q.charge_at("k1", 10, t0));
        assert!(q.charge_at("k2", 10, t0));
        assert!(!q.charge_at("k3", 10, t0), "new key refused when at max_keys");
        assert!(q.charge_at("k1", 20, t0), "existing key still allowed");
        assert!(
            q.charge_at("k3", 10, t0 + Duration::from_secs(60)),
            "new key allowed once earlier windows expire"
        );
    }

    #[test]
    fn only_put_objects_spends_the_quota() {
        let p = VssProxy::new("http://h/vss", 10, Duration::from_secs(1)).unwrap().with_quota(10);
        assert!(p.allow_write("getObject", "t", 1_000_000));
        assert!(p.allow_write("deleteObject", "t", 1_000_000));
        assert!(p.allow_write("putObjects", "t", 10));
        assert!(!p.allow_write("putObjects", "t", 1));
        let open = VssProxy::new("http://h/vss", 10, Duration::from_secs(1)).unwrap().with_quota(0);
        assert!(open.allow_write("putObjects", "t", usize::MAX));
    }

    #[test]
    fn a_good_file_parses_and_unknown_fields_are_dropped() {
        let info = with(|_| {}).unwrap();
        assert_eq!(info.port, 9735);
        assert_eq!(info.fee, Fee { ppm: 10000, min_sat: 2000 });
        let served: Value = serde_json::to_value(&info).unwrap();
        assert!(served.get("extra").is_none());
        // Optional fields may be left out.
        let info = with(|v| {
            v.as_object_mut().unwrap().remove("lsps2_token");
            v.as_object_mut().unwrap().remove("message");
        })
        .unwrap();
        assert_eq!((info.lsps2_token, info.message), (None, None));
        // Upper-case hex is accepted and served lower-case.
        assert_eq!(with(|v| v["node_id"] = json!(NODE_ID.to_uppercase())).unwrap().node_id, NODE_ID);
        for host in ["127.0.0.1", "lsp-1.fortis.example", "[::1]", "::1"] {
            assert!(with(|v| v["host"] = json!(host)).is_ok(), "{host}");
        }
        for net in NETWORKS {
            assert!(with(|v| v["network"] = json!(net)).is_ok(), "{net}");
        }
        assert!(with(|v| v["fee"]["ppm"] = json!(100_000)).is_ok());
        assert!(with(|v| { v["min_payment_sat"] = json!(5); v["max_payment_sat"] = json!(5) }).is_ok());
    }

    #[test]
    fn every_rule_is_enforced_and_named() {
        type Edit = Box<dyn FnOnce(&mut Value)>;
        let cases: Vec<(Edit, &str)> = vec![
            (Box::new(|v| v["node_id"] = json!("02abcd")), "node_id"),
            (Box::new(|v| v["node_id"] = json!(format!("04{}", &NODE_ID[2..]))), "node_id"),
            (Box::new(|v| v["node_id"] = json!(format!("{}g", &NODE_ID[..65]))), "node_id"),
            (Box::new(|v| v["host"] = json!("")), "host"),
            (Box::new(|v| v["host"] = json!("https://lsp.example.org")), "host"),
            (Box::new(|v| v["host"] = json!("lsp.example.org/path")), "host"),
            (Box::new(|v| v["host"] = json!("lsp example.org")), "host"),
            (Box::new(|v| v["host"] = json!("lsp.example.org\n")), "host"),
            (Box::new(|v| v["port"] = json!(0)), "port"),
            (Box::new(|v| v["port"] = json!(65536)), "port"),
            (Box::new(|v| v["port"] = json!(-1)), "ln info"),
            (Box::new(|v| v["fee"]["ppm"] = json!(100_001)), "fee.ppm"),
            (Box::new(|v| v["min_payment_sat"] = json!(250_001)), "min_payment_sat"),
            (Box::new(|v| v["channel_min_sat"] = json!(1_000_001)), "channel_min_sat"),
            (Box::new(|v| v["network"] = json!("mainnet")), "network"),
            (Box::new(|v| v["lsps2_token"] = json!("")), "lsps2_token"),
            (Box::new(|v| v["message"] = json!("x".repeat(1001))), "message"),
            (Box::new(|v| { v.as_object_mut().unwrap().remove("enabled"); }), "enabled"),
            (Box::new(|v| v["enabled"] = json!("yes")), "ln info"),
        ];
        for (f, what) in cases {
            let e = with(f).expect_err(what);
            assert!(format!("{e:#}").contains(what), "{what}: {e:#}");
        }
        assert!(parse_info(b"{ not json").is_err());
    }

    #[test]
    fn the_token_is_never_echoed_in_an_error() {
        let e = with(|v| v["lsps2_token"] = json!("secret-tok\u{7}")).unwrap_err();
        assert!(!format!("{e:#}").contains("secret-tok"), "{e:#}");
    }

    #[test]
    fn an_oversized_file_is_refused() {
        let d = TempDir::new("ln-info-big");
        std::fs::create_dir_all(&*d).unwrap();
        let p = d.join("info.json");
        let mut v = good();
        v["padding"] = json!("x".repeat(INFO_FILE_MAX as usize));
        std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        let e = LnInfoSource::open(&p).err().expect("too big");
        assert!(format!("{e:#}").contains("KiB"), "{e:#}");
    }

    #[test]
    fn reload_takes_a_good_change_and_keeps_the_last_good_on_a_bad_one() {
        let d = TempDir::new("ln-info-poll");
        std::fs::create_dir_all(&*d).unwrap();
        let p = d.join("info.json");
        std::fs::write(&p, serde_json::to_vec(&good()).unwrap()).unwrap();
        let src = LnInfoSource::open(&p).unwrap();
        assert_eq!(src.poll_once(), None, "unchanged");
        assert!(src.info().enabled);

        let mut off = good();
        off["enabled"] = json!(false);
        off["message"] = json!("paused");
        // Different size, so the change shows even on a coarse-mtime fs.
        std::fs::write(&p, serde_json::to_vec_pretty(&off).unwrap()).unwrap();
        assert_eq!(src.poll_once(), Some(true));
        assert!(!src.info().enabled);
        let served: Value = serde_json::from_slice(&src.body()).unwrap();
        assert_eq!(served["message"], "paused");

        std::fs::write(&p, b"{ broken").unwrap();
        assert_eq!(src.poll_once(), Some(false));
        assert_eq!(src.poll_once(), None, "a rejected version is not re-read every poll");
        assert!(!src.info().enabled, "last good kept");

        std::fs::remove_file(&p).unwrap();
        assert_eq!(src.poll_once(), Some(false));
        assert!(!src.info().enabled, "a vanished file keeps the last good one");

        std::fs::write(&p, serde_json::to_vec(&good()).unwrap()).unwrap();
        assert_eq!(src.poll_once(), Some(true));
        assert!(src.info().enabled);
    }

    #[test]
    fn only_the_four_vss_calls_pass_and_only_as_post() {
        for op in VSS_OPS {
            assert_eq!(vss_op(&Method::Post, op).ok(), Some(op));
            assert!(matches!(vss_op(&Method::Get, op), Err(Reply::Json(405, _))), "GET {op}");
            assert!(matches!(vss_op(&Method::Put, op), Err(Reply::Json(405, _))), "PUT {op}");
        }
        for rest in [
            "", "foo", "../x", "..", ".", "./getObject", "%2e%2e/x", "%2E%2E/getObject", "/getObject",
            "getObject/", "getObject/x", "getobject", "GETOBJECT", "%67etObject", "getObject%00",
            "getObject?x", "getObject\n", "x/../getObject",
        ] {
            assert!(matches!(vss_op(&Method::Post, rest), Err(Reply::Json(404, _))), "{rest:?}");
        }
    }

    #[test]
    fn content_type_is_passed_only_when_sane() {
        assert_eq!(safe_content_type(Some("application/x-protobuf")), "application/x-protobuf");
        assert_eq!(safe_content_type(None), "application/octet-stream");
        assert_eq!(safe_content_type(Some("")), "application/octet-stream");
        assert_eq!(safe_content_type(Some("a\r\nX-Evil: 1")), "application/octet-stream");
        assert_eq!(safe_content_type(Some(&"a".repeat(200))), "application/octet-stream");
    }

    #[test]
    fn the_proxy_wants_an_http_url_and_a_positive_cap() {
        let t = Duration::from_secs(1);
        assert!(VssProxy::new("http://127.0.0.1:8080/vss/", VSS_MAX_BODY_DEFAULT, t).is_ok());
        assert_eq!(VssProxy::new("http://h/vss/", 1, t).unwrap().base, "http://h/vss");
        assert!(VssProxy::new("127.0.0.1:8080", 1, t).is_err());
        assert!(VssProxy::new("file:///etc", 1, t).is_err());
        assert!(VssProxy::new("http://h", 0, t).is_err());
        assert_eq!(VSS_MAX_BODY_DEFAULT, 4 * 1024 * 1024);
    }
}
