//! UniFFI bindings for the fortis wallet core — Kotlin (Android) and Swift (iOS).
//!
//! Mirrors `wallet-wasm`: key handling, address derivation, coin selection,
//! P2WPKH / `SIGHASH_UNIFIED` signing, and seed sealing. The native shell adds
//! secure storage (Keychain / Secure Enclave, Keystore / StrongBox), biometrics,
//! the chain backend (Esplora / fortisd), and the UI.

uniffi::setup_scaffolding!();

use std::str::FromStr;
use std::sync::Mutex;

use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::bip32::Xpub;
use wallet_core::bitcoin::{consensus, Address, Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use wallet_core::crypto::{self, KdfParams};
use wallet_core::{ChainParams, MasterKey};

// `flat_error`: the binding carries just the `Display` string, so Kotlin/Swift
// `exception.message` is the message itself — not uniffi's `v1=…` field dump.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum FfiError {
    #[error("{0}")]
    Wallet(String),
}

impl From<wallet_core::WalletError> for FfiError {
    fn from(err: wallet_core::WalletError) -> Self {
        FfiError::Wallet(err.to_string())
    }
}
fn err(msg: impl std::fmt::Display) -> FfiError {
    FfiError::Wallet(msg.to_string())
}
type Result<T> = std::result::Result<T, FfiError>;

fn params(chain: &str, network: &str) -> Result<ChainParams> {
    let c = match chain {
        "btc" => wallet_core::Chain::Btc,
        "xbt" => wallet_core::Chain::Xbt,
        _ => return Err(err("chain must be \"btc\" or \"xbt\"")),
    };
    ChainParams::resolve(c, network)
        .ok_or_else(|| err(format!("unknown chain/network {chain}/{network}")))
}

// ---------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------

#[derive(uniffi::Record)]
pub struct AddressInfo {
    pub address: String,
    pub script_pubkey_hex: String,
}

#[derive(uniffi::Record)]
pub struct Indices {
    pub next_receive: u32,
    pub next_change: u32,
}

/// A coin the shell reports from its chain backend (Esplora / fortisd).
#[derive(uniffi::Record, Clone)]
pub struct WalletUtxo {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub confirmations: u32,
    pub derivation_index: u32,
    pub is_change: bool,
}

impl WalletUtxo {
    fn to_core(&self) -> Result<wallet_core::Utxo> {
        Ok(wallet_core::Utxo {
            outpoint: OutPoint::new(
                Txid::from_str(&self.txid).map_err(|_| err(format!("bad txid {}", self.txid)))?,
                self.vout,
            ),
            value: Amount::from_sat(self.value_sat),
            script_pubkey: spk(&self.script_pubkey_hex)?,
            confirmations: self.confirmations,
            derivation_index: self.derivation_index,
            is_change: self.is_change,
        })
    }
}

#[derive(uniffi::Record)]
pub struct PayTo {
    pub address: String,
    pub amount_sat: u64,
}

/// A hosted backend's pricing (from its status endpoint). Self-hosted backends
/// don't report one, and no fee is charged.
#[derive(uniffi::Record)]
pub struct ServiceFee {
    pub address: String,
    pub bps: u32,
    pub floor_sat: u64,
    pub cap_sat: u64,
}

#[derive(uniffi::Record)]
pub struct SpentInput {
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub derivation_index: u32,
    pub is_change: bool,
}

#[derive(uniffi::Record)]
pub struct SelectedInput {
    pub txid: String,
    pub vout: u32,
    pub value_sat: u64,
    pub script_pubkey_hex: String,
    pub derivation_index: u32,
    pub is_change: bool,
}

/// Coin-selection result. Sign `tx_hex`'s inputs with `Wallet::sign_funding_tx`,
/// mapping `selected` to `SpentInput`s.
#[derive(uniffi::Record)]
pub struct FundingPlan {
    pub tx_hex: String,
    pub fee_sat: u64,
    pub change_sat: Option<u64>,
    pub service_fee_sat: Option<u64>,
    pub selected: Vec<SelectedInput>,
}

impl FundingPlan {
    fn from_core(p: &wallet_core::FundingPlan) -> Self {
        FundingPlan {
            tx_hex: hex::encode(consensus::serialize(&p.tx)),
            fee_sat: p.fee.to_sat(),
            change_sat: p.change.map(|c| c.to_sat()),
            service_fee_sat: p.service_fee.map(|c| c.to_sat()),
            selected: p
                .selected
                .iter()
                .map(|u| SelectedInput {
                    txid: u.outpoint.txid.to_string(),
                    vout: u.outpoint.vout,
                    value_sat: u.value.to_sat(),
                    script_pubkey_hex: hex::encode(u.script_pubkey.as_bytes()),
                    derivation_index: u.derivation_index,
                    is_change: u.is_change,
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// free functions
// ---------------------------------------------------------------------------

/// A `words`-word mnemonic (12 or 24). `csprng` is ≥ 32 bytes from
/// `SecureRandom`. `extra` (optional) is additional entropy the shell collected —
/// device-motion sensor noise, touch jitter, timing jitter — folded into the
/// CSPRNG bytes (see `wallet_core::entropy`); it can only strengthen the seed.
#[uniffi::export]
pub fn generate_mnemonic(csprng: Vec<u8>, extra: Option<Vec<u8>>, words: u8) -> Result<String> {
    let mut sources: Vec<&[u8]> = Vec::new();
    if let Some(e) = extra.as_deref() {
        if !e.is_empty() {
            sources.push(e);
        }
    }
    let (mnemonic, _key) = MasterKey::generate_mixed(&csprng, &sources, words)?;
    Ok(mnemonic.to_string())
}

/// The 2048-word BIP-39 English wordlist, standard order — for a recovery-phrase
/// autocomplete in the shell.
#[uniffi::export]
pub fn bip39_wordlist() -> Vec<String> {
    wallet_core::bip39_wordlist().iter().map(|w| w.to_string()).collect()
}

/// Seal a mnemonic under a password (Argon2id → XChaCha20-Poly1305). `salt` ≥ 8
/// bytes, `nonce` exactly 24 fresh random bytes — both stored beside the blob.
#[uniffi::export]
pub fn seal_mnemonic_with_password(
    mnemonic: String,
    password: String,
    salt: Vec<u8>,
    nonce: Vec<u8>,
) -> Result<String> {
    let nonce: [u8; 24] = nonce.try_into().map_err(|_| err("nonce must be 24 bytes"))?;
    let kek = crypto::kek_from_password(password.as_bytes(), &salt, &KdfParams::default())?;
    Ok(hex::encode(crypto::seal(mnemonic.as_bytes(), &kek, &nonce)?))
}

#[uniffi::export]
pub fn unseal_mnemonic_with_password(
    blob_hex: String,
    password: String,
    salt: Vec<u8>,
) -> Result<String> {
    let kek = crypto::kek_from_password(password.as_bytes(), &salt, &KdfParams::default())?;
    let pt = crypto::unseal(&hex::decode(&blob_hex).map_err(err)?, &kek)?;
    String::from_utf8(pt.to_vec()).map_err(|_| err("wrong password or corrupt data"))
}

// ---------------------------------------------------------------------------
// Wallet — holds the master key
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct Wallet {
    key: MasterKey,
    network: String,
}

#[uniffi::export]
impl Wallet {
    /// Restore from a BIP-39 phrase. `network` is `"mainnet"` or `"regtest"`.
    #[uniffi::constructor]
    pub fn from_mnemonic(
        mnemonic: String,
        passphrase: String,
        network: String,
    ) -> Result<std::sync::Arc<Self>> {
        Ok(std::sync::Arc::new(Wallet {
            key: MasterKey::from_phrase(&mnemonic, &passphrase)?,
            network,
        }))
    }

    pub fn account_xpub(&self, chain: String, account: u32) -> Result<String> {
        Ok(self
            .key
            .account_xpub(&params(&chain, &self.network)?, account)?
            .to_string())
    }

    /// Master key fingerprint (8 hex chars) — descriptor key-origin for a
    /// watch-only import.
    pub fn master_fingerprint(&self) -> String {
        self.key.master_fingerprint().to_string()
    }

    pub fn swap_pubkey(&self, chain: String, account: u32, swap_index: u32) -> Result<String> {
        let pk = self
            .key
            .swap_pubkey(&params(&chain, &self.network)?, account, swap_index)?;
        Ok(hex::encode(pk.serialize()))
    }

    /// DER signature + sighash-flag byte (hex) for an HTLC leg.
    pub fn sign_swap(
        &self,
        chain: String,
        account: u32,
        swap_index: u32,
        sighash_hex: String,
    ) -> Result<String> {
        let msg: [u8; 32] = hex::decode(&sighash_hex)
            .map_err(err)?
            .try_into()
            .map_err(|_| err("sighash must be 32 bytes"))?;
        let sig = self
            .key
            .sign_swap(&params(&chain, &self.network)?, account, swap_index, &msg)?;
        Ok(hex::encode(sig))
    }

    /// Sign every P2WPKH input of `tx_hex` (a `FundingPlan` transaction). `spent`
    /// is the plan's `selected` list. Returns the fully-signed transaction hex.
    pub fn sign_funding_tx(
        &self,
        chain: String,
        account: u32,
        tx_hex: String,
        spent: Vec<SpentInput>,
    ) -> Result<String> {
        let p = params(&chain, &self.network)?;
        let mut tx: Transaction =
            consensus::deserialize(&hex::decode(&tx_hex).map_err(err)?).map_err(err)?;
        let prevouts: Vec<TxOut> = spent
            .iter()
            .map(|s| {
                Ok(TxOut {
                    value: Amount::from_sat(s.value_sat),
                    script_pubkey: spk(&s.script_pubkey_hex)?,
                })
            })
            .collect::<Result<_>>()?;
        let paths: Vec<(bool, u32)> =
            spent.iter().map(|s| (s.is_change, s.derivation_index)).collect();
        self.key.sign_p2wpkh_tx(&p, account, &mut tx, &prevouts, &paths)?;
        Ok(hex::encode(consensus::serialize(&tx)))
    }
}

// ---------------------------------------------------------------------------
// WalletView — watch-only derivation + coin selection
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct WalletView {
    inner: Mutex<wallet_core::WalletView>,
    net: wallet_core::bitcoin::Network,
}

#[uniffi::export]
impl WalletView {
    #[uniffi::constructor]
    pub fn new(
        chain: String,
        network: String,
        account_xpub: String,
    ) -> Result<std::sync::Arc<Self>> {
        let p = params(&chain, &network)?;
        let xpub = Xpub::from_str(&account_xpub).map_err(|_| err("invalid account xpub"))?;
        Ok(std::sync::Arc::new(WalletView {
            net: p.network,
            inner: Mutex::new(wallet_core::WalletView::new(p, xpub)),
        }))
    }

    pub fn set_next_indices(&self, next_receive: u32, next_change: u32) {
        self.inner
            .lock()
            .unwrap()
            .set_next_indices(next_receive, next_change);
    }

    pub fn next_indices(&self) -> Indices {
        let (next_receive, next_change) = self.inner.lock().unwrap().next_indices();
        Indices { next_receive, next_change }
    }

    pub fn address_at(&self, branch: u32, index: u32) -> Result<AddressInfo> {
        let a = self.inner.lock().unwrap().address_at(branch, index)?;
        Ok(AddressInfo {
            script_pubkey_hex: hex::encode(a.script_pubkey().as_bytes()),
            address: a.to_string(),
        })
    }

    /// Check that `address` parses and is valid on this view's network, returning
    /// its canonical string form. Cheap (no derivation) — call it to validate a
    /// send recipient before doing any network I/O, so a bad address reports a
    /// clear error instead of being masked by a later "no coins" check.
    pub fn check_address(&self, address: String) -> Result<String> {
        Ok(parse_address(&address, self.net)?.to_string())
    }

    /// `op_return` (optional): bytes for a 0-value `OP_RETURN` appended to the tx.
    /// On the Bitcoin chain, ~100 random bytes make the tx consensus-invalid on
    /// the BLAKE2b fork (over its 82-byte datacarrier cap) — replay protection.
    /// `service_fee` (optional, from the backend's status) appends the hosted
    /// backend's fee output. `fee_from_amount`: carve the network + service fee
    /// out of the amount (the recipient gets `amount − fees`) rather than adding
    /// them on top.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_payment(
        &self,
        utxos: Vec<WalletUtxo>,
        outputs: Vec<PayTo>,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        op_return: Option<Vec<u8>>,
        service_fee: Option<ServiceFee>,
        fee_from_amount: bool,
    ) -> Result<FundingPlan> {
        let coins = to_core_utxos(&utxos)?;
        let mut outs: Vec<TxOut> = outputs
            .iter()
            .map(|o| {
                Ok(TxOut {
                    value: Amount::from_sat(o.amount_sat),
                    script_pubkey: address_spk(&o.address, self.net)?,
                })
            })
            .collect::<Result<_>>()?;
        if let Some(data) = op_return.filter(|d| !d.is_empty()) {
            outs.push(wallet_core::op_return_output(&data)?);
        }
        let sf = to_core_service_fee(&service_fee, self.net)?;
        let plan = self.inner.lock().unwrap().plan_payment(
            &coins,
            outs,
            feerate_sat_vb,
            min_confirmations,
            sf.as_ref(),
            fee_from_amount,
        )?;
        Ok(FundingPlan::from_core(&plan))
    }

    /// `service_fee` (optional) carves the hosted backend's fee out of the swept amount.
    pub fn plan_sweep(
        &self,
        utxos: Vec<WalletUtxo>,
        dest_address: String,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        service_fee: Option<ServiceFee>,
    ) -> Result<FundingPlan> {
        let coins = to_core_utxos(&utxos)?;
        let dest = address_spk(&dest_address, self.net)?;
        let sf = to_core_service_fee(&service_fee, self.net)?;
        let plan = self.inner.lock().unwrap().plan_sweep(
            &coins,
            dest,
            feerate_sat_vb,
            min_confirmations,
            sf.as_ref(),
        )?;
        Ok(FundingPlan::from_core(&plan))
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn spk(hex_str: &str) -> Result<ScriptBuf> {
    Ok(ScriptBuf::from(hex::decode(hex_str).map_err(err)?))
}

fn to_core_utxos(utxos: &[WalletUtxo]) -> Result<Vec<wallet_core::Utxo>> {
    utxos.iter().map(|u| u.to_core()).collect()
}

fn parse_address(addr: &str, net: wallet_core::bitcoin::Network) -> Result<Address> {
    // Paste often carries a trailing newline/space; rust-bitcoin then reports a
    // baffling "base58 error" for what is really a valid bech32 address.
    let addr = addr.trim();
    Address::<NetworkUnchecked>::from_str(addr)
        .map_err(|_| err(format!("\"{addr}\" is not a valid address")))?
        .require_network(net)
        .map_err(|_| err(format!("address {addr} is not valid on this network")))
}

fn address_spk(addr: &str, net: wallet_core::bitcoin::Network) -> Result<ScriptBuf> {
    Ok(parse_address(addr, net)?.script_pubkey())
}

fn to_core_service_fee(
    sf: &Option<ServiceFee>,
    net: wallet_core::bitcoin::Network,
) -> Result<Option<wallet_core::ServiceFee>> {
    sf.as_ref()
        .map(|sf| {
            Ok(wallet_core::ServiceFee {
                bps: sf.bps,
                floor_sat: sf.floor_sat,
                cap_sat: sf.cap_sat,
                fee_spk: address_spk(&sf.address, net)?,
            })
        })
        .transpose()
}
