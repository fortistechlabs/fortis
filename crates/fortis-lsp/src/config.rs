//! `lsp.toml`: the service's configuration. See `lsp.toml.example`.

use std::net::SocketAddr;
use std::path::PathBuf;

use std::str::FromStr;

use ldk_node::bitcoin::Network;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::liquidity::LSPS2ServiceConfig;

use crate::outbound::{self, Outbound, RawOutbound};

pub type Result<T> = std::result::Result<T, String>;

/// Where `fortis-lsp` looks for its config without `--config`.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/fortis/lsp.toml";

/// LSPS2 promises: the channel stays open at least this many blocks after it
/// confirms (~30 days). Not configurable in phase 1.
pub const MIN_CHANNEL_LIFETIME_BLOCKS: u32 = 4_320;
/// LSPS2: the largest `to_self_delay` a client may ask of us (BOLT 2's
/// recommended upper bound).
pub const MAX_CLIENT_TO_SELF_DELAY: u32 = 2_016;

/// The service configuration. Built only by [`LspConfig::from_toml`], which
/// validates it. `Debug` redacts `token`; the bitcoind RPC password is never
/// part of it (read on demand from `rpc_password_file`).
#[derive(Clone, PartialEq)]
pub struct LspConfig {
    /// `bitcoin` (mainnet), `testnet`, `signet` or `regtest`.
    pub network: Network,
    /// Lightning P2P listen address.
    pub listen: SocketAddr,
    /// bitcoind RPC `host:port`.
    pub bitcoind_rpc: String,
    pub rpc_user: String,
    pub rpc_password_file: PathBuf,
    /// Absolute. Holds the node's seed (`keys_seed`), its SQLite store and logs.
    pub data_dir: PathBuf,
    /// Channel-open fee, proportional to the payment (ppm).
    pub open_fee_ppm: u32,
    /// Channel-open fee floor.
    pub open_fee_min_sat: u64,
    pub min_payment_size_msat: u64,
    pub max_payment_size_msat: u64,
    /// JIT channel size = forwarded amount × this.
    pub channel_size_factor: f64,
    pub channel_min_sat: u64,
    pub channel_max_sat: u64,
    /// LSPS2 token the clients must present, if any.
    pub token: Option<String>,
    /// Log a warning while the on-chain balance is below this.
    pub low_liquidity_sat: u64,
    /// Prometheus `/metrics` (and `/info`) listener.
    pub metrics_listen: SocketAddr,
    /// Must be `true` for a non-loopback `metrics_listen`.
    pub allow_public_metrics: bool,
    /// Public node name (at most 32 bytes). Needed to announce channels.
    pub alias: Option<String>,
    /// Where other nodes reach us (`host:port` or `ip:port`), announced in
    /// the node announcement. Not `listen`: that is a local bind address.
    pub announce_addresses: Vec<SocketAddress>,
    /// Announced channels to routing nodes, opened at most once per run
    /// when missing (see `outbound.rs`).
    pub outbound: Vec<Outbound>,
    /// What this node charges to forward a payment, on every channel: base
    /// (msat per payment) and proportional (millionths). Both 0 by default,
    /// so a user's payment through the LSP costs nothing here (1 sat arrives
    /// as 1 sat); hops beyond the LSP charge their own.
    pub forwarding_fee_base_msat: u32,
    pub forwarding_fee_ppm: u32,
}

/// The TOML as written: every key optional (defaults below), no unknown keys.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Raw {
    network: Option<String>,
    listen: Option<SocketAddr>,
    bitcoind_rpc: Option<String>,
    rpc_user: Option<String>,
    rpc_password_file: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    open_fee_ppm: Option<u32>,
    open_fee_min_sat: Option<u64>,
    min_payment_size_msat: Option<u64>,
    max_payment_size_msat: Option<u64>,
    channel_size_factor: Option<f64>,
    channel_min_sat: Option<u64>,
    channel_max_sat: Option<u64>,
    token: Option<String>,
    low_liquidity_sat: Option<u64>,
    metrics_listen: Option<SocketAddr>,
    allow_public_metrics: Option<bool>,
    alias: Option<String>,
    announce_addresses: Option<Vec<String>>,
    #[serde(default)]
    outbound: Vec<RawOutbound>,
    forwarding_fee_base_msat: Option<u32>,
    forwarding_fee_ppm: Option<u32>,
}

impl LspConfig {
    /// Parses and validates `lsp.toml`. Missing keys take the defaults
    /// documented in `lsp.toml.example`.
    pub fn from_toml(s: &str) -> Result<LspConfig> {
        let raw: Raw = toml::from_str(s).map_err(|e| format!("lsp.toml: {e}"))?;
        let network = match raw.network.as_deref() {
            None => Network::Bitcoin,
            Some(n) => n
                .parse::<Network>()
                .map_err(|e| format!("network {n:?}: {e}"))?,
        };
        let channel_max_sat = raw.channel_max_sat.unwrap_or(500_000);
        let announce_addresses = raw
            .announce_addresses
            .unwrap_or_default()
            .iter()
            .map(|a| {
                SocketAddress::from_str(a)
                    .map_err(|e| format!("announce_addresses: {a:?} is not host:port ({e:?})"))
            })
            .collect::<Result<Vec<_>>>()?;
        let outbound = outbound::parse_all(&raw.outbound)?;
        let max_default = channel_max_sat
            .checked_mul(1_000)
            .ok_or("channel_max_sat is too large")?;
        let c = LspConfig {
            network,
            listen: raw
                .listen
                .unwrap_or_else(|| "0.0.0.0:9735".parse().unwrap()),
            bitcoind_rpc: raw.bitcoind_rpc.unwrap_or_else(|| "127.0.0.1:8332".into()),
            rpc_user: raw.rpc_user.unwrap_or_else(|| "fortis-lsp".into()),
            rpc_password_file: raw
                .rpc_password_file
                .unwrap_or_else(|| "/etc/fortis/lsp-rpc-password".into()),
            data_dir: raw.data_dir.unwrap_or_else(|| "/var/lib/fortis-lsp".into()),
            open_fee_ppm: raw.open_fee_ppm.unwrap_or(10_000),
            open_fee_min_sat: raw.open_fee_min_sat.unwrap_or(1_000),
            min_payment_size_msat: raw.min_payment_size_msat.unwrap_or(10_000 * 1_000),
            max_payment_size_msat: raw.max_payment_size_msat.unwrap_or(max_default),
            channel_size_factor: raw.channel_size_factor.unwrap_or(2.0),
            channel_min_sat: raw.channel_min_sat.unwrap_or(100_000),
            channel_max_sat,
            token: raw.token,
            low_liquidity_sat: raw.low_liquidity_sat.unwrap_or(2_000_000),
            metrics_listen: raw
                .metrics_listen
                .unwrap_or_else(|| "127.0.0.1:9736".parse().unwrap()),
            allow_public_metrics: raw.allow_public_metrics.unwrap_or(false),
            alias: raw.alias.map(|a| a.trim().to_string()),
            announce_addresses,
            outbound,
            forwarding_fee_base_msat: raw.forwarding_fee_base_msat.unwrap_or(0),
            forwarding_fee_ppm: raw.forwarding_fee_ppm.unwrap_or(0),
        };
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        if self.min_payment_size_msat > self.max_payment_size_msat {
            return Err(format!(
                "min_payment_size_msat ({}) > max_payment_size_msat ({})",
                self.min_payment_size_msat, self.max_payment_size_msat
            ));
        }
        if self.channel_min_sat > self.channel_max_sat {
            return Err(format!(
                "channel_min_sat ({}) > channel_max_sat ({})",
                self.channel_min_sat, self.channel_max_sat
            ));
        }
        let f = self.channel_size_factor;
        if !f.is_finite() || !(1.0..=MAX_CHANNEL_SIZE_FACTOR).contains(&f) {
            return Err(format!(
                "channel_size_factor ({f}) must be a number from 1.0 to {MAX_CHANNEL_SIZE_FACTOR}"
            ));
        }
        if self.max_payment_size_msat > MAX_PAYMENT_CAP_MSAT {
            return Err(format!(
                "max_payment_size_msat ({}) is above 21 000 000 BTC",
                self.max_payment_size_msat
            ));
        }
        if self.open_fee_ppm > 100_000 {
            return Err(format!(
                "open_fee_ppm ({}) is above 100000 (10%)",
                self.open_fee_ppm
            ));
        }
        if self.open_fee_min_sat.checked_mul(1_000).is_none() {
            return Err("open_fee_min_sat is too large".into());
        }
        if self.data_dir.as_os_str().is_empty() || !self.data_dir.is_absolute() {
            return Err(format!(
                "data_dir ({}) must be an absolute path",
                self.data_dir.display()
            ));
        }
        if !self.metrics_listen.ip().is_loopback() && !self.allow_public_metrics {
            return Err(format!(
                "metrics_listen ({}) is not a loopback address; set allow_public_metrics = true to allow it",
                self.metrics_listen
            ));
        }
        self.rpc_host_port()?;
        effective_payment_bounds(self)?;
        if self.token.as_deref() == Some("") {
            return Err("token must not be empty (omit it for no token)".into());
        }
        if let Some(a) = &self.alias {
            if a.is_empty() || a.len() > 32 || a.chars().any(char::is_control) {
                return Err(format!(
                    "alias ({a:?}) must be 1..=32 bytes of printable text"
                ));
            }
        }
        if !self.announce_addresses.is_empty() && self.alias.is_none() {
            return Err(
                "announce_addresses needs an alias (ldk-node announces both together)".into(),
            );
        }
        if self.announce_addresses.len() > 10 {
            return Err("announce_addresses: at most 10".into());
        }
        if self.forwarding_fee_ppm > 100_000 {
            return Err(format!(
                "forwarding_fee_ppm ({}) is above 100000 (10%)",
                self.forwarding_fee_ppm
            ));
        }
        if self.forwarding_fee_base_msat > 100_000_000 {
            return Err(format!(
                "forwarding_fee_base_msat ({}) is above 100000000 (100 000 sat)",
                self.forwarding_fee_base_msat
            ));
        }
        if !self.outbound.is_empty() && (self.alias.is_none() || self.announce_addresses.is_empty())
        {
            return Err(
                "[[outbound]] channels are announced: set alias and announce_addresses too".into(),
            );
        }
        Ok(())
    }

    /// `bitcoind_rpc` split into host and port.
    pub fn rpc_host_port(&self) -> Result<(String, u16)> {
        let bad = || format!("bitcoind_rpc ({:?}) must be host:port", self.bitcoind_rpc);
        let (host, port) = self.bitcoind_rpc.rsplit_once(':').ok_or_else(bad)?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port: u16 = port.parse().map_err(|_| bad())?;
        if host.is_empty() || port == 0 {
            return Err(bad());
        }
        Ok((host.to_string(), port))
    }

    /// The bitcoind RPC password: the first line of `rpc_password_file`,
    /// trimmed. Never logged.
    pub fn rpc_password(&self) -> Result<String> {
        let s = std::fs::read_to_string(&self.rpc_password_file).map_err(|e| {
            format!(
                "rpc_password_file {}: {e}",
                self.rpc_password_file.display()
            )
        })?;
        let pw = s.lines().next().unwrap_or("").trim().to_string();
        if pw.is_empty() {
            return Err(format!(
                "rpc_password_file {} is empty",
                self.rpc_password_file.display()
            ));
        }
        Ok(pw)
    }

    /// ldk-node's LSPS2 service parameters. `channel_min_sat` /
    /// `channel_max_sat` have no counterpart in ldk-node 0.7.1 (see README).
    pub fn lsps2_service_config(&self) -> LSPS2ServiceConfig {
        LSPS2ServiceConfig {
            require_token: self.token.clone(),
            advertise_service: false,
            channel_opening_fee_ppm: self.open_fee_ppm,
            channel_over_provisioning_ppm: over_provisioning_ppm(self.channel_size_factor)
                .expect("validated"),
            min_channel_opening_fee_msat: self.open_fee_min_sat * 1_000,
            min_channel_lifetime: MIN_CHANNEL_LIFETIME_BLOCKS,
            max_client_to_self_delay: MAX_CLIENT_TO_SELF_DELAY,
            min_payment_size_msat: self.payment_bounds().0,
            max_payment_size_msat: self.payment_bounds().1,
            // Zero-conf JIT channels: the client trusts us not to
            // double-spend the funding transaction (spec, LSPS2 bLIP-52).
            client_trusts_lsp: true,
        }
    }

    /// [`effective_payment_bounds`] of this (validated) config: what the
    /// LSP really accepts and publishes.
    pub fn payment_bounds(&self) -> (u64, u64) {
        effective_payment_bounds(self).expect("validated in from_toml")
    }
}

/// The most bitcoin there is, in msat: `max_payment_size_msat`'s cap.
pub const MAX_PAYMENT_CAP_MSAT: u64 = 21_000_000 * 100_000_000 * 1_000;
/// `channel_size_factor`'s cap.
pub const MAX_CHANNEL_SIZE_FACTOR: f64 = 10.0;

/// ldk-node 0.7.1's JIT channel size (sat) for a payment of `payment_msat`,
/// or `None` when ldk-node would not open one (nothing left to forward) or
/// its own `u64` arithmetic would overflow. Mirrors
/// lightning-liquidity 0.2.7 `compute_opening_fee` (fee = max(ceil(P·ppm /
/// 10⁶), min fee)), its `amt_to_forward_msat = P − fee`, and ldk-node's
/// `OpenChannel` handler (`liquidity.rs`): channel = (amt + amt·op_ppm /
/// 10⁶) / 1000, all integer, rounding down.
fn jit_channel_sat(c: &LspConfig, op_ppm: u32, payment_msat: u64) -> Option<u64> {
    let fee = payment_msat
        .checked_mul(c.open_fee_ppm as u64)?
        .checked_add(999_999)?
        / 1_000_000;
    let fee = fee.max(c.open_fee_min_sat.checked_mul(1_000)?);
    let amt = payment_msat.saturating_sub(fee);
    if amt == 0 {
        return None;
    }
    let over = amt.checked_mul(op_ppm as u64)? / 1_000_000;
    Some(amt.checked_add(over)? / 1_000)
}

/// Payment bounds `(min_msat, max_msat)` that keep every JIT channel ldk-node
/// opens within `channel_min_sat..=channel_max_sat` (ldk-node 0.7.1 has no
/// channel size setting; it sizes the channel from the payment).
///
/// - max: the largest P with P·(10⁶ + op_ppm)/10⁶ ≤ channel_max_sat·1000,
///   rounded down — a payment never yields a channel above P × factor — and
///   at most the configured `max_payment_size_msat`.
/// - min: the smallest P ≥ the configured `min_payment_size_msat` whose
///   channel (exact ldk-node maths, fee included) is ≥ channel_min_sat;
///   the channel size is non-decreasing in P (fee ppm < 10⁶), so a binary
///   search finds it.
///
/// `Err` when no payment size satisfies both.
pub fn effective_payment_bounds(c: &LspConfig) -> Result<(u64, u64)> {
    let op_ppm = over_provisioning_ppm(c.channel_size_factor).ok_or_else(|| {
        format!(
            "channel_size_factor ({}) out of range",
            c.channel_size_factor
        )
    })?;
    let cap = (c.channel_max_sat as u128)
        .checked_mul(1_000 * 1_000_000)
        .ok_or("channel_max_sat is too large")?
        / (1_000_000 + op_ppm as u128);
    let max = cap.min(c.max_payment_size_msat as u128) as u64;
    let none = || {
        format!(
            "no payment size gives a JIT channel within channel_min_sat..channel_max_sat ({}..{} sat) \
             with min/max_payment_size_msat {}..{}, open_fee_ppm {}, open_fee_min_sat {}, channel_size_factor {}",
            c.channel_min_sat,
            c.channel_max_sat,
            c.min_payment_size_msat,
            c.max_payment_size_msat,
            c.open_fee_ppm,
            c.open_fee_min_sat,
            c.channel_size_factor
        )
    };
    let ok = |p: u64| jit_channel_sat(c, op_ppm, p).is_some_and(|ch| ch >= c.channel_min_sat);
    if c.min_payment_size_msat > max || !ok(max) {
        return Err(none());
    }
    // ldk-node's arithmetic must not overflow anywhere in the range: `max`
    // is the largest operand and every step is monotone in P.
    if jit_channel_sat(c, op_ppm, max).is_none() {
        return Err("max_payment_size_msat too large for ldk-node's fee arithmetic".into());
    }
    let (mut lo, mut hi) = (c.min_payment_size_msat, max);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok((lo, max))
}

/// `factor` as ldk-node's over-provisioning on top of the forwarded amount
/// (factor 2.0 = +100 % = 1 000 000 ppm). `None` when it does not fit.
fn over_provisioning_ppm(factor: f64) -> Option<u32> {
    let ppm = ((factor - 1.0) * 1_000_000.0).round();
    (ppm >= 0.0 && ppm <= u32::MAX as f64).then_some(ppm as u32)
}

impl std::fmt::Debug for LspConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspConfig")
            .field("network", &self.network)
            .field("listen", &self.listen)
            .field("bitcoind_rpc", &self.bitcoind_rpc)
            .field("rpc_user", &self.rpc_user)
            .field("rpc_password_file", &self.rpc_password_file)
            .field("data_dir", &self.data_dir)
            .field("open_fee_ppm", &self.open_fee_ppm)
            .field("open_fee_min_sat", &self.open_fee_min_sat)
            .field("min_payment_size_msat", &self.min_payment_size_msat)
            .field("max_payment_size_msat", &self.max_payment_size_msat)
            .field("channel_size_factor", &self.channel_size_factor)
            .field("channel_min_sat", &self.channel_min_sat)
            .field("channel_max_sat", &self.channel_max_sat)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("low_liquidity_sat", &self.low_liquidity_sat)
            .field("metrics_listen", &self.metrics_listen)
            .field("allow_public_metrics", &self.allow_public_metrics)
            .field("alias", &self.alias)
            .field("announce_addresses", &self.announce_addresses)
            .field("outbound", &self.outbound)
            .field("forwarding_fee_base_msat", &self.forwarding_fee_base_msat)
            .field("forwarding_fee_ppm", &self.forwarding_fee_ppm)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(extra: &str) -> Result<LspConfig> {
        LspConfig::from_toml(extra)
    }

    #[test]
    fn defaults_match_spec() {
        let c = with("").unwrap();
        assert_eq!(c.open_fee_ppm, 10_000);
        assert_eq!(c.open_fee_min_sat, 1_000);
        assert_eq!(c.channel_min_sat, 100_000);
        assert_eq!(c.channel_max_sat, 500_000);
        assert_eq!(c.channel_size_factor, 2.0);
        assert_eq!(c.min_payment_size_msat, 10_000 * 1_000);
        assert_eq!(c.max_payment_size_msat, 500_000 * 1_000);
        assert_eq!(c.low_liquidity_sat, 2_000_000);
        assert_eq!(c.listen, "0.0.0.0:9735".parse().unwrap());
        assert_eq!(c.metrics_listen, "127.0.0.1:9736".parse().unwrap());
        assert_eq!(c.network, Network::Bitcoin);
        assert_eq!(c.token, None);
        assert!(!c.allow_public_metrics);
    }

    #[test]
    fn example_config_is_the_defaults() {
        let example = LspConfig::from_toml(include_str!("../lsp.toml.example")).unwrap();
        assert_eq!(example, with("").unwrap());
    }

    #[test]
    fn rejects_min_greater_than_max() {
        let e = with("min_payment_size_msat = 2000\nmax_payment_size_msat = 1000").unwrap_err();
        assert!(e.contains("min_payment_size_msat"), "{e}");
        assert!(
            with("min_payment_size_msat = 100000000\nmax_payment_size_msat = 100000000").is_ok()
        );
    }

    #[test]
    fn rejects_channel_min_greater_than_channel_max() {
        let e = with("channel_min_sat = 600000").unwrap_err();
        assert!(e.contains("channel_min_sat"), "{e}");
    }

    #[test]
    fn rejects_factor_below_one_and_non_finite() {
        assert!(with("channel_size_factor = 0.99")
            .unwrap_err()
            .contains("channel_size_factor"));
        assert!(with("channel_size_factor = nan").is_err());
        assert!(with("channel_size_factor = inf").is_err());
        assert!(with("channel_size_factor = 1.0").is_ok());
    }

    #[test]
    fn rejects_ppm_above_ten_percent() {
        assert!(with("open_fee_ppm = 100001")
            .unwrap_err()
            .contains("open_fee_ppm"));
        assert!(with("open_fee_ppm = 100000").is_ok());
    }

    #[test]
    fn rejects_empty_or_relative_data_dir() {
        assert!(with("data_dir = \"\"").unwrap_err().contains("data_dir"));
        assert!(with("data_dir = \"var/lib/lsp\"")
            .unwrap_err()
            .contains("data_dir"));
        assert!(with("data_dir = \"/srv/lsp\"").is_ok());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(with("open_fee_pmm = 5")
            .unwrap_err()
            .contains("open_fee_pmm"));
        // A password in the file itself is an unknown key, not a feature.
        assert!(with("rpc_password = \"hunter2\"").is_err());
    }

    #[test]
    fn rejects_public_metrics_unless_allowed() {
        assert!(with("metrics_listen = \"0.0.0.0:9736\"")
            .unwrap_err()
            .contains("metrics_listen"));
        assert!(with("metrics_listen = \"[::]:9736\"").is_err());
        assert!(with("metrics_listen = \"[::1]:9736\"").is_ok());
        assert!(with("metrics_listen = \"0.0.0.0:9736\"\nallow_public_metrics = true").is_ok());
    }

    #[test]
    fn parses_network_and_rejects_bad_ones() {
        assert_eq!(
            with("network = \"regtest\"").unwrap().network,
            Network::Regtest
        );
        assert!(with("network = \"moon\"").is_err());
    }

    #[test]
    fn rejects_bad_bitcoind_rpc_address() {
        assert!(with("bitcoind_rpc = \"localhost\"").is_err());
        assert_eq!(
            with("bitcoind_rpc = \"10.0.0.2:18443\"")
                .unwrap()
                .rpc_host_port()
                .unwrap(),
            ("10.0.0.2".into(), 18443)
        );
    }

    #[test]
    fn debug_redacts_token() {
        let c = with("token = \"s3cret-token\"").unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains("s3cret-token"), "{d}");
        assert!(d.contains("<redacted>"), "{d}");
    }

    const ACINQ: &str = "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f";

    #[test]
    fn forwarding_is_free_by_default_and_bounded() {
        let c = with("").unwrap();
        assert_eq!((c.forwarding_fee_base_msat, c.forwarding_fee_ppm), (0, 0));
        let c = with("forwarding_fee_base_msat = 1000\nforwarding_fee_ppm = 100").unwrap();
        assert_eq!(
            (c.forwarding_fee_base_msat, c.forwarding_fee_ppm),
            (1_000, 100)
        );
        assert!(with("forwarding_fee_ppm = 100001")
            .unwrap_err()
            .contains("forwarding_fee_ppm"));
        assert!(with("forwarding_fee_base_msat = 100000001")
            .unwrap_err()
            .contains("forwarding_fee_base_msat"));
    }

    #[test]
    fn announcement_and_outbound_are_off_by_default() {
        let c = with("").unwrap();
        assert_eq!(c.alias, None);
        assert!(c.announce_addresses.is_empty());
        assert!(c.outbound.is_empty());
    }

    #[test]
    fn parses_alias_announce_addresses_and_outbound() {
        let c = with(&format!(
            "alias = \" Fortis \"\nannounce_addresses = [\"lsp.fortistechlabs.com:9735\", \"34.31.126.17:9735\"]\n\n[[outbound]]\npeer = \"{ACINQ}@3.33.236.230:9735\"\nsat = 150000\n"
        ))
        .unwrap();
        assert_eq!(c.alias.as_deref(), Some("Fortis"));
        let addrs: Vec<String> = c.announce_addresses.iter().map(|a| a.to_string()).collect();
        assert_eq!(addrs, ["lsp.fortistechlabs.com:9735", "34.31.126.17:9735"]);
        assert_eq!(c.outbound.len(), 1);
        assert_eq!(c.outbound[0].node_id.to_string(), ACINQ);
        assert_eq!(c.outbound[0].sat, 150_000);
    }

    #[test]
    fn outbound_needs_alias_and_announce_addresses() {
        let ob = format!("[[outbound]]\npeer = \"{ACINQ}@3.33.236.230:9735\"\nsat = 150000\n");
        let e = with(&ob).unwrap_err();
        assert!(e.contains("alias and announce_addresses"), "{e}");
        let e = with(&format!("alias = \"Fortis\"\n{ob}")).unwrap_err();
        assert!(e.contains("alias and announce_addresses"), "{e}");
        let e = with("announce_addresses = [\"34.31.126.17:9735\"]").unwrap_err();
        assert!(e.contains("needs an alias"), "{e}");
    }

    #[test]
    fn rejects_bad_alias_and_addresses() {
        assert!(with("alias = \"\"").unwrap_err().contains("alias"));
        assert!(with("alias = \"this alias is far longer than thirty-two bytes\"").is_err());
        let e = with("alias = \"F\"\nannounce_addresses = [\"no-port\"]").unwrap_err();
        assert!(e.contains("announce_addresses"), "{e}");
        let e = with(&format!("alias = \"F\"\nannounce_addresses = [\"1.2.3.4:9735\"]\n[[outbound]]\npeer = \"{ACINQ}@1.2.3.4:9735\"\nsat = 5\n")).unwrap_err();
        assert!(e.contains("sat"), "{e}");
    }

    #[test]
    fn lsps2_service_config_maps_the_fields() {
        let c = with("token = \"t\"\nchannel_size_factor = 2.5\nopen_fee_min_sat = 3000").unwrap();
        let s = c.lsps2_service_config();
        assert_eq!(s.channel_opening_fee_ppm, 10_000);
        assert_eq!(s.min_channel_opening_fee_msat, 3_000_000);
        // Effective bounds: (P − max(1 % P, 3 000 sat)) × 2.5 >= 100 000 sat
        // -> P >= 43 000 sat; P × 2.5 <= 500 000 sat -> P <= 200 000 sat.
        assert_eq!(s.min_payment_size_msat, 43_000_000);
        assert_eq!(s.max_payment_size_msat, 200_000_000);
        // factor 2.5 = forward amount + 150 % over-provisioning.
        assert_eq!(s.channel_over_provisioning_ppm, 1_500_000);
        assert_eq!(s.require_token.as_deref(), Some("t"));
        assert!(s.client_trusts_lsp);
        assert!(!s.advertise_service);
    }

    #[test]
    fn reads_the_rpc_password_from_its_file_only() {
        let dir = std::env::temp_dir().join(format!("fortis-lsp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pw");
        std::fs::write(&f, "pa55word\n").unwrap();
        let c = with(&format!("rpc_password_file = \"{}\"", f.display())).unwrap();
        assert_eq!(c.rpc_password().unwrap(), "pa55word");
        std::fs::write(&f, "\n").unwrap();
        assert!(c.rpc_password().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(c.rpc_password().is_err());
    }

    /// ldk-node 0.7.1's JIT channel size for a payment, transcribed from
    /// lightning-liquidity 0.2.7 `compute_opening_fee` / service.rs and
    /// ldk-node `liquidity.rs` (`OpenChannel`), independently of config.rs.
    fn ldk_channel_sat(c: &LspConfig, payment_msat: u64) -> u64 {
        let ppm = ((c.channel_size_factor - 1.0) * 1_000_000.0).round() as u64;
        let fee = (payment_msat * c.open_fee_ppm as u64).div_ceil(1_000_000);
        let fee = fee.max(c.open_fee_min_sat * 1_000);
        let amt_to_forward_msat = payment_msat.saturating_sub(fee);
        let over_provisioning_msat = (amt_to_forward_msat * ppm) / 1_000_000;
        (amt_to_forward_msat + over_provisioning_msat) / 1000
    }

    #[test]
    fn effective_bounds_with_spec_defaults() {
        let c = with("").unwrap();
        // (P − max(1 % P, 1 000 sat)) × 2 >= 100 000 sat -> P = 51 000 sat;
        // P × 2 <= 500 000 sat -> P = 250 000 sat.
        assert_eq!(
            effective_payment_bounds(&c).unwrap(),
            (51_000_000, 250_000_000)
        );
        assert_eq!(ldk_channel_sat(&c, 51_000_000), 100_000);
        assert_eq!(ldk_channel_sat(&c, 50_999_999), 99_999);
        let s = c.lsps2_service_config();
        assert_eq!(
            (s.min_payment_size_msat, s.max_payment_size_msat),
            (51_000_000, 250_000_000)
        );
    }

    #[test]
    fn every_payment_in_bounds_gives_a_channel_in_bounds() {
        for extra in [
            "",
            "channel_size_factor = 1.0",
            "channel_size_factor = 1.5",
            "channel_size_factor = 3.3333333",
            "channel_size_factor = 10.0\nchannel_max_sat = 5000000",
            "open_fee_ppm = 0\nopen_fee_min_sat = 0",
            "open_fee_ppm = 100000\nopen_fee_min_sat = 0",
            "open_fee_ppm = 77777\nopen_fee_min_sat = 5000\nchannel_size_factor = 1.25",
            "channel_min_sat = 20000\nchannel_max_sat = 1000000\nopen_fee_min_sat = 333",
            "min_payment_size_msat = 1\nopen_fee_ppm = 1\nopen_fee_min_sat = 1",
        ] {
            let c = with(extra).unwrap_or_else(|e| panic!("{extra:?}: {e}"));
            let (min, max) = effective_payment_bounds(&c).unwrap();
            assert!(c.min_payment_size_msat <= min && min <= max && max <= c.max_payment_size_msat);
            let step = ((max - min) / 997).max(1);
            let mut payments: Vec<u64> = (min..=max).step_by(step as usize).collect();
            payments.extend([min, min + 1, max - 1, max]);
            for p in payments {
                let ch = ldk_channel_sat(&c, p);
                assert!(
                    (c.channel_min_sat..=c.channel_max_sat).contains(&ch),
                    "{extra:?}: payment {p} msat -> channel {ch} sat"
                );
            }
            // Tight where derived (not clamped by the configured bounds).
            if min > c.min_payment_size_msat {
                assert!(
                    ldk_channel_sat(&c, min - 1) < c.channel_min_sat,
                    "{extra:?}: min not tight"
                );
            }
        }
    }

    #[test]
    fn configured_min_above_the_derived_min_wins() {
        let c = with("min_payment_size_msat = 80000000").unwrap();
        assert_eq!(
            effective_payment_bounds(&c).unwrap(),
            (80_000_000, 250_000_000)
        );
        let c = with("max_payment_size_msat = 200000000").unwrap();
        assert_eq!(
            effective_payment_bounds(&c).unwrap(),
            (51_000_000, 200_000_000)
        );
    }

    #[test]
    fn rejects_a_config_with_no_valid_payment_range() {
        // Max payment below the smallest payment that reaches channel_min_sat.
        let e = with("max_payment_size_msat = 40000000").unwrap_err();
        assert!(e.contains("no payment size"), "{e}");
        // channel_min = channel_max: the open fee leaves no payment exactly on it.
        assert!(with("channel_min_sat = 500000").is_err());
        // channel_min unreachable with a 100 % fee... not possible (ppm <= 10 %);
        // a huge fee floor instead.
        assert!(with("open_fee_min_sat = 400000").is_err());
    }

    #[test]
    fn rejects_factor_above_ten_and_absurd_payment_sizes() {
        assert!(with("channel_size_factor = 10.1")
            .unwrap_err()
            .contains("channel_size_factor"));
        assert!(with("channel_size_factor = 10.0\nchannel_max_sat = 5000000").is_ok());
        let e = with("max_payment_size_msat = 2100000000000000001").unwrap_err();
        assert!(e.contains("max_payment_size_msat"), "{e}");
    }
}
