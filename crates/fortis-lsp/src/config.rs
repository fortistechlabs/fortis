//! `lsp.toml`: the service's configuration. See `lsp.toml.example`.

use std::net::SocketAddr;
use std::path::PathBuf;

use ldk_node::bitcoin::Network;
use ldk_node::liquidity::LSPS2ServiceConfig;

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
        if !f.is_finite() || f < 1.0 || over_provisioning_ppm(f).is_none() {
            return Err(format!(
                "channel_size_factor ({f}) must be a finite number >= 1.0"
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
        if self.token.as_deref() == Some("") {
            return Err("token must not be empty (omit it for no token)".into());
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
            min_payment_size_msat: self.min_payment_size_msat,
            max_payment_size_msat: self.max_payment_size_msat,
            // Zero-conf JIT channels: the client trusts us not to
            // double-spend the funding transaction (spec, LSPS2 bLIP-52).
            client_trusts_lsp: true,
        }
    }

    /// The largest JIT channel ldk-node can open under this config:
    /// `max_payment_size_msat` forwarded × `channel_size_factor`.
    pub fn largest_jit_channel_sat(&self) -> u64 {
        ((self.max_payment_size_msat / 1_000) as f64 * self.channel_size_factor) as u64
    }
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
        assert!(with("min_payment_size_msat = 1000\nmax_payment_size_msat = 1000").is_ok());
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

    #[test]
    fn lsps2_service_config_maps_the_fields() {
        let c = with("token = \"t\"\nchannel_size_factor = 2.5\nopen_fee_min_sat = 3000").unwrap();
        let s = c.lsps2_service_config();
        assert_eq!(s.channel_opening_fee_ppm, 10_000);
        assert_eq!(s.min_channel_opening_fee_msat, 3_000_000);
        assert_eq!(s.min_payment_size_msat, 10_000_000);
        assert_eq!(s.max_payment_size_msat, 500_000_000);
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
}
