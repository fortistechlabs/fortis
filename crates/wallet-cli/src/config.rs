//! On-disk wallet + node configuration (`<home>/wallet.json`).
//!
//! The file holds **no secret material** — only the BIP-84 account xpub, the master
//! fingerprint (for descriptor key-origin), the local address counters, and how to
//! reach the node. The seed is shown once at `init` and never written here.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Bitcoin Knots data directory — the `.cookie` file lives here.
    pub datadir: String,
    /// Base RPC URL, e.g. `http://127.0.0.1:8332`.
    pub rpc_url: String,
    /// Name of the watch-only wallet fortis manages on the node.
    pub watch_wallet: String,
    /// Explicit cookie file. Defaults to `<datadir>/.cookie` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie_file: Option<String>,
    /// Static RPC credentials — used only when there is no cookie file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_password: Option<String>,
}

impl NodeConfig {
    /// `user:password` string for HTTP Basic auth against the node.
    pub fn auth_userpass(&self, network: &str) -> Result<String> {
        if let Some(path) = &self.cookie_file {
            let s = fs::read_to_string(path)
                .with_context(|| format!("reading cookie {path}"))?;
            return Ok(s.trim().to_string());
        }
        match fortis_node::cookie_auth(&self.datadir, network) {
            Ok(s) => Ok(s),
            Err(e) => {
                if let (Some(u), Some(p)) = (&self.rpc_user, &self.rpc_password) {
                    Ok(format!("{u}:{p}"))
                } else {
                    Err(e)
                }
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletConfig {
    pub version: u32,
    /// `"xbt"` (the BLAKE2b fork) or `"btc"`.
    pub chain: String,
    /// `"mainnet"`, `"regtest"`, or `"regtest-legacy"`.
    pub network: String,
    /// BIP-84 account xpub at `m/84'/<coin>'/0'`.
    pub account_xpub: String,
    /// Master key fingerprint (8 hex chars) for descriptor key-origin.
    pub master_fingerprint: String,
    /// Next unused external (receive) address index.
    pub next_receive: u32,
    /// Next unused internal (change) address index.
    pub next_change: u32,
    pub node: NodeConfig,
}

impl WalletConfig {
    pub fn path(home: &Path) -> PathBuf {
        home.join("wallet.json")
    }

    pub fn exists(home: &Path) -> bool {
        Self::path(home).exists()
    }

    pub fn load(home: &Path) -> Result<Self> {
        let p = Self::path(home);
        let bytes = fs::read(&p).with_context(|| {
            format!("no wallet at {} — run `fortis init` first", p.display())
        })?;
        let cfg: WalletConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", p.display()))?;
        if cfg.version > CONFIG_VERSION {
            return Err(anyhow!(
                "wallet.json is version {} but this build understands up to {}",
                cfg.version,
                CONFIG_VERSION
            ));
        }
        Ok(cfg)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        fs::create_dir_all(home)
            .with_context(|| format!("creating {}", home.display()))?;
        let p = Self::path(home);
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        fs::write(&p, json).with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }
}

/// `$FORTIS_HOME`, else `%APPDATA%\fortis` (Windows) / `~/.fortis` (Unix).
pub fn default_home() -> PathBuf {
    if let Ok(h) = std::env::var("FORTIS_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("fortis")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join(".fortis")
    }
}

/// The platform-default Bitcoin data directory (a starting guess for `init`).
pub fn default_bitcoin_datadir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Bitcoin")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join(".bitcoin")
    }
}
