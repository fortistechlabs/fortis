//! fortisd settings (from CLI flags) + persisted state (`<home>/fortisd.json`) and
//! the API token (`<home>/fortisd.token`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Settings {
    pub datadir: String,
    pub rpc_url: String,
    pub network: String,
    pub cookie_file: Option<String>,
    pub allow_origin: String,
    pub esplora_proxy: Option<String>,
    pub home: PathBuf,
    pub pricing: Option<Pricing>,
}

/// A service fee the operator charges on sends made through this gateway.
/// Reported in `/v1/status` so clients add the fee output themselves;
/// `/v1/broadcast` requires at least `floor_sat` paid to `address` as a basic
/// integrity check. Self-hosters simply don't set this — it costs nothing.
#[derive(Debug, Clone, Serialize)]
pub struct Pricing {
    pub address: String,
    pub bps: u32,
    pub floor_sat: u64,
    pub cap_sat: u64,
}

/// The account this gateway is currently serving (learned from `POST /v1/connect`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connected {
    pub chain: String,
    pub network: String,
    pub account_xpub: String,
    pub master_fingerprint: String,
    pub watch_wallet: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub connected: Option<Connected>,
}

impl State {
    fn path(home: &Path) -> PathBuf {
        home.join("fortisd.json")
    }

    pub fn load(home: &Path) -> State {
        fs::read(Self::path(home))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        fs::create_dir_all(home)?;
        let mut b = serde_json::to_vec_pretty(self)?;
        b.push(b'\n');
        fs::write(Self::path(home), b).context("writing fortisd.json")?;
        Ok(())
    }
}

/// Load `<home>/fortisd.token`, creating a fresh random one if absent or too short.
/// Returns `(token, is_new)`.
pub fn load_or_create_token(home: &Path) -> Result<(String, bool)> {
    let p = home.join("fortisd.token");
    if let Ok(s) = fs::read_to_string(&p) {
        let t = s.trim().to_string();
        if t.len() >= 32 {
            return Ok((t, false));
        }
    }
    let mut raw = [0u8; 24];
    getrandom::getrandom(&mut raw).map_err(|e| anyhow!("CSPRNG failed: {e}"))?;
    let token = hex::encode(raw);
    fs::create_dir_all(home)?;
    fs::write(&p, format!("{token}\n")).context("writing fortisd.token")?;
    Ok((token, true))
}

pub fn default_home() -> PathBuf {
    if let Ok(h) = std::env::var("FORTISD_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("fortisd")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".fortisd")
    }
}

pub fn default_bitcoin_datadir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| ".".into())).join("Bitcoin")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".bitcoin")
    }
}
