//! Optional at-rest sealing of the wallet seed (`<home>/seed.enc`).
//!
//! XChaCha20-Poly1305 over an Argon2id-stretched password — both from
//! [`wallet_core::crypto`]. The sealed plaintext is `{"mnemonic","passphrase"}`
//! JSON so a BIP-39 passphrase round-trips together with the words.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use wallet_core::crypto::{self, KdfParams};
use wallet_core::MasterKey;
use zeroize::Zeroizing;

const SEALED_FILE: &str = "seed.enc";

#[derive(Serialize, Deserialize)]
struct SealedFile {
    version: u32,
    kdf: String,
    salt: String,
    m_cost_kib: u32,
    t_cost: u32,
    p_cost: u32,
    /// `nonce(24) || ciphertext || tag`, hex.
    blob: String,
}

#[derive(Serialize, Deserialize)]
struct Plain {
    mnemonic: String,
    #[serde(default)]
    passphrase: String,
}

pub fn sealed_path(home: &Path) -> PathBuf {
    home.join(SEALED_FILE)
}

pub fn is_sealed(home: &Path) -> bool {
    sealed_path(home).exists()
}

/// Where a signing command should get the seed.
#[derive(Clone, Copy)]
pub enum SeedSource {
    /// Decrypt `seed.enc` (prompts for the password).
    Sealed,
    /// Read mnemonic (line 1) then optional passphrase (line 2) from stdin.
    Stdin,
    /// `Sealed` when `seed.enc` exists, otherwise `Stdin`.
    Auto,
}

fn rand_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|e| anyhow!("CSPRNG failed: {e}"))?;
    Ok(b)
}

/// Seal `mnemonic` (+ optional `passphrase`) under `password` to `<home>/seed.enc`.
pub fn seal(home: &Path, mnemonic: &str, passphrase: &str, password: &str) -> Result<()> {
    let params = KdfParams::default();
    let salt = rand_bytes::<16>()?;
    let nonce = rand_bytes::<24>()?;
    let kek = crypto::kek_from_password(password.as_bytes(), &salt, &params)
        .map_err(|e| anyhow!("{e}"))?;

    let plain = Zeroizing::new(serde_json::to_vec(&Plain {
        mnemonic: mnemonic.to_string(),
        passphrase: passphrase.to_string(),
    })?);
    let blob = crypto::seal(&plain, &kek, &nonce).map_err(|e| anyhow!("{e}"))?;

    let file = SealedFile {
        version: 1,
        kdf: "argon2id".into(),
        salt: hex::encode(salt),
        m_cost_kib: params.m_cost_kib,
        t_cost: params.t_cost,
        p_cost: params.p_cost,
        blob: hex::encode(&blob),
    };
    fs::create_dir_all(home)?;
    let mut bytes = serde_json::to_vec_pretty(&file)?;
    bytes.push(b'\n');
    fs::write(sealed_path(home), bytes).context("writing seed.enc")?;
    Ok(())
}

pub fn delete(home: &Path) -> Result<()> {
    let p = sealed_path(home);
    if p.exists() {
        fs::remove_file(&p).context("removing seed.enc")?;
    }
    Ok(())
}

fn unseal(home: &Path, password: &str) -> Result<(Zeroizing<String>, Zeroizing<String>)> {
    let bytes = fs::read(sealed_path(home)).context("reading seed.enc")?;
    let file: SealedFile = serde_json::from_slice(&bytes).context("parsing seed.enc")?;
    if file.version != 1 {
        bail!("seed.enc version {} is not supported", file.version);
    }
    let salt = hex::decode(&file.salt).context("seed.enc salt")?;
    let blob = hex::decode(&file.blob).context("seed.enc blob")?;
    let params = KdfParams {
        m_cost_kib: file.m_cost_kib,
        t_cost: file.t_cost,
        p_cost: file.p_cost,
    };
    let kek = crypto::kek_from_password(password.as_bytes(), &salt, &params)
        .map_err(|e| anyhow!("{e}"))?;
    let plain = crypto::unseal(&blob, &kek)
        .map_err(|_| anyhow!("wrong password, or seed.enc is corrupt"))?;
    let p: Plain = serde_json::from_slice(&plain).context("decrypted seed payload")?;
    Ok((Zeroizing::new(p.mnemonic), Zeroizing::new(p.passphrase)))
}

/// The sealing password: `$FORTIS_SEED_PASSWORD` if set (for scripting), else the
/// terminal prompt.
fn read_password(prompt: &str) -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var("FORTIS_SEED_PASSWORD") {
        if !p.is_empty() {
            return Ok(Zeroizing::new(p));
        }
    }
    Ok(Zeroizing::new(
        rpassword::prompt_password(prompt).context("reading password")?,
    ))
}

/// Load the master key according to `source`.
pub fn load_master_key(home: &Path, source: SeedSource) -> Result<MasterKey> {
    let source = match source {
        SeedSource::Auto if is_sealed(home) => SeedSource::Sealed,
        SeedSource::Auto => SeedSource::Stdin,
        s => s,
    };
    let (mnemonic, passphrase) = match source {
        SeedSource::Sealed => {
            if !is_sealed(home) {
                bail!(
                    "no sealed seed at {} — run `fortis import-seed`, or pass --phrase-stdin",
                    sealed_path(home).display()
                );
            }
            let password = read_password("seed password: ")?;
            unseal(home, &password)?
        }
        SeedSource::Stdin => {
            let stdin = io::stdin();
            let mut lines = stdin.lock().lines();
            let mnemonic = lines
                .next()
                .transpose()?
                .ok_or_else(|| anyhow!("expected a recovery phrase on stdin"))?;
            let mnemonic =
                Zeroizing::new(mnemonic.split_whitespace().collect::<Vec<_>>().join(" "));
            let passphrase = Zeroizing::new(
                lines.next().transpose()?.unwrap_or_default().trim().to_string(),
            );
            (mnemonic, passphrase)
        }
        SeedSource::Auto => unreachable!(),
    };
    MasterKey::from_phrase(&mnemonic, &passphrase).map_err(|e| anyhow!("{e}"))
}

/// A new sealing password: `$FORTIS_SEED_PASSWORD` if set, else prompt twice.
pub fn prompt_new_password() -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var("FORTIS_SEED_PASSWORD") {
        if !p.is_empty() {
            if p.chars().count() < 8 {
                bail!("FORTIS_SEED_PASSWORD must be at least 8 characters");
            }
            return Ok(Zeroizing::new(p));
        }
    }
    let a = rpassword::prompt_password("new seed password: ").context("reading password")?;
    if a.chars().count() < 8 {
        bail!("use a password of at least 8 characters");
    }
    let b = rpassword::prompt_password("confirm password:  ").context("reading password")?;
    if a != b {
        bail!("passwords do not match");
    }
    Ok(Zeroizing::new(a))
}

/// Read a recovery phrase from a visible stdin prompt.
pub fn prompt_mnemonic() -> Result<Zeroizing<String>> {
    eprint!("recovery phrase: ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() < 12 {
        bail!("expected at least 12 words, got {}", words.len());
    }
    Ok(Zeroizing::new(words.join(" ")))
}

/// Read an optional BIP-39 passphrase from a visible stdin prompt.
pub fn prompt_passphrase() -> Result<Zeroizing<String>> {
    eprint!("BIP-39 passphrase (empty for none): ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(Zeroizing::new(line.trim_end_matches(['\r', '\n']).to_string()))
}
