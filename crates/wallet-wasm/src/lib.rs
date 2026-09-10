//! wasm-bindgen surface for `wallet-core`. Keys live in wasm linear memory; JS gets
//! opaque handles (`Wallet`, `WalletView`, `Htlc`) plus hex/JSON in and out.

use wasm_bindgen::prelude::*;

use wallet_core::bitcoin::address::NetworkUnchecked;
use wallet_core::bitcoin::hashes::{sha256, Hash};
use wallet_core::bitcoin::secp256k1::PublicKey;
use wallet_core::bitcoin::{
    consensus, Address, Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid,
};
use wallet_core::crypto::{self, KdfParams};
use wallet_core::swap::UnsignedSpend;
use wallet_core::{
    Chain, ChainParams, HtlcContract, MasterKey, SwapEvent, SwapMachine, SwapParams, SwapRole,
    SwapState,
};

mod dto;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

fn js<E: core::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

std::thread_local! {
    static NETWORK: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Select the network for every subsequent call: `"mainnet"` (default) or
/// `"regtest"`. Call once after `init()`.
#[wasm_bindgen(js_name = setNetwork)]
pub fn set_network(name: &str) -> Result<(), JsError> {
    if !matches!(name, "mainnet" | "bitcoin" | "regtest" | "regtest-legacy") {
        return Err(JsError::new(
            "network must be \"mainnet\", \"regtest\", or \"regtest-legacy\"",
        ));
    }
    NETWORK.with(|n| *n.borrow_mut() = name.to_string());
    Ok(())
}

fn network() -> String {
    NETWORK.with(|n| {
        let v = n.borrow();
        if v.is_empty() { "mainnet".into() } else { v.clone() }
    })
}

fn params(chain: &str) -> Result<ChainParams, JsError> {
    let c = match chain {
        "btc" => Chain::Btc,
        "xbt" => Chain::Xbt,
        _ => return Err(JsError::new("chain must be \"btc\" or \"xbt\"")),
    };
    ChainParams::resolve(c, &network()).ok_or_else(|| JsError::new("unknown network"))
}

fn pubkey(hex_str: &str) -> Result<PublicKey, JsError> {
    PublicKey::from_slice(&hex::decode(hex_str).map_err(js)?).map_err(js)
}

fn bytes32(hex_str: &str) -> Result<[u8; 32], JsError> {
    hex::decode(hex_str)
        .map_err(js)?
        .try_into()
        .map_err(|_| JsError::new("expected 32 bytes"))
}

fn spk(hex_str: &str) -> Result<ScriptBuf, JsError> {
    Ok(ScriptBuf::from(hex::decode(hex_str).map_err(js)?))
}

fn tx_from_hex(hex_str: &str) -> Result<Transaction, JsError> {
    consensus::deserialize(&hex::decode(hex_str).map_err(js)?).map_err(js)
}

fn tx_to_hex(tx: &Transaction) -> String {
    hex::encode(consensus::serialize(tx))
}

/// Build a `words`-word mnemonic (12 or 24). `csprng` is ≥ 32 bytes from
/// `crypto.getRandomValues`. `extra` (optional) is any additional entropy the
/// shell collected — pointer/touch jitter, timing jitter, dice — folded into the
/// CSPRNG bytes (see `wallet_core::entropy`); it can only strengthen the seed.
#[wasm_bindgen(js_name = generateMnemonic)]
pub fn generate_mnemonic(csprng: &[u8], extra: Option<Vec<u8>>, words: u8) -> Result<String, JsError> {
    let mut sources: Vec<&[u8]> = Vec::new();
    if let Some(e) = extra.as_deref() {
        if !e.is_empty() {
            sources.push(e);
        }
    }
    let (mnemonic, _key) = MasterKey::generate_mixed(csprng, &sources, words).map_err(js)?;
    Ok(mnemonic.to_string())
}

/// The 2048-word BIP-39 English wordlist, standard order — for a recovery-phrase
/// autocomplete in the UI.
#[wasm_bindgen(js_name = bip39Wordlist)]
pub fn bip39_wordlist() -> Vec<String> {
    wallet_core::bip39_wordlist().iter().map(|w| w.to_string()).collect()
}

/// Holds the master key in wasm memory. JS never sees the seed.
#[wasm_bindgen]
pub struct Wallet {
    key: MasterKey,
}

#[wasm_bindgen]
impl Wallet {
    /// Restore from a BIP-39 phrase (+ optional passphrase, `""` if none).
    #[wasm_bindgen(constructor)]
    pub fn new(mnemonic: &str, passphrase: &str) -> Result<Wallet, JsError> {
        Ok(Wallet { key: MasterKey::from_phrase(mnemonic, passphrase).map_err(js)? })
    }

    /// BIP-84 account xpub (`m/84'/coin'/account'`) — pass to `new WalletView(...)`.
    #[wasm_bindgen(js_name = accountXpub)]
    pub fn account_xpub(&self, chain: &str, account: u32) -> Result<String, JsError> {
        Ok(self.key.account_xpub(&params(chain)?, account).map_err(js)?.to_string())
    }

    /// Master key fingerprint (8 hex chars) — the descriptor key-origin the gateway
    /// needs for `POST /v1/connect`.
    #[wasm_bindgen(js_name = masterFingerprint)]
    pub fn master_fingerprint(&self) -> String {
        self.key.master_fingerprint().to_string()
    }

    /// Compressed swap public key (hex), `m/84'/coin'/account'/2'/index'`.
    #[wasm_bindgen(js_name = swapPubkey)]
    pub fn swap_pubkey(&self, chain: &str, account: u32, swap_index: u32) -> Result<String, JsError> {
        let pk = self.key.swap_pubkey(&params(chain)?, account, swap_index).map_err(js)?;
        Ok(hex::encode(pk.serialize()))
    }

    /// ECDSA-sign a 32-byte sighash (hex) with a swap key. Returns DER sig + hashtype
    /// byte (hex) for `Htlc.finalizeRedeem` / `finalizeRefund`.
    #[wasm_bindgen(js_name = signSwap)]
    pub fn sign_swap(
        &self,
        chain: &str,
        account: u32,
        swap_index: u32,
        sighash_hex: &str,
    ) -> Result<String, JsError> {
        let sig = self
            .key
            .sign_swap(&params(chain)?, account, swap_index, &bytes32(sighash_hex)?)
            .map_err(js)?;
        Ok(hex::encode(sig))
    }

    /// Sign every input of an unsigned P2WPKH-funded transaction (e.g. the HTLC
    /// funding tx from `WalletView.planHtlcFunding`). `spent` is the `selected` array
    /// from the funding plan. Returns the fully-signed tx hex, ready to broadcast.
    #[wasm_bindgen(js_name = signFundingTx)]
    pub fn sign_funding_tx(
        &self,
        chain: &str,
        account: u32,
        tx_hex: &str,
        spent: JsValue,
    ) -> Result<String, JsError> {
        let spent: Vec<dto::JsSpentInput> = serde_wasm_bindgen::from_value(spent).map_err(js)?;
        let mut tx = tx_from_hex(tx_hex)?;
        let prevouts: Vec<TxOut> = spent
            .iter()
            .map(|s| {
                Ok(TxOut {
                    value: Amount::from_sat(s.value_sat),
                    script_pubkey: spk(&s.script_pubkey_hex)?,
                })
            })
            .collect::<Result<_, JsError>>()?;
        let paths: Vec<(bool, u32)> =
            spent.iter().map(|s| (s.is_change, s.derivation_index)).collect();
        self.key
            .sign_p2wpkh_tx(&params(chain)?, account, &mut tx, &prevouts, &paths)
            .map_err(js)?;
        Ok(tx_to_hex(&tx))
    }
}

/// Watch-only per-chain view: address derivation and coin selection. No secrets.
#[wasm_bindgen]
pub struct WalletView {
    inner: wallet_core::WalletView,
    chain: String,
}

#[wasm_bindgen]
impl WalletView {
    #[wasm_bindgen(constructor)]
    pub fn new(chain: &str, account_xpub: &str) -> Result<WalletView, JsError> {
        let xpub = account_xpub.parse().map_err(|_| JsError::new("invalid account xpub"))?;
        Ok(WalletView {
            inner: wallet_core::WalletView::new(params(chain)?, xpub),
            chain: chain.to_string(),
        })
    }

    /// Resume derivation counters after a reload so the next address is not one
    /// already handed out (persist these client-side).
    #[wasm_bindgen(js_name = setNextIndices)]
    pub fn set_next_indices(&mut self, next_receive: u32, next_change: u32) {
        self.inner.set_next_indices(next_receive, next_change);
    }

    /// `{ next_receive, next_change }` — read back to persist after planning a payment.
    #[wasm_bindgen(js_name = nextIndices)]
    pub fn next_indices(&self) -> Result<JsValue, JsError> {
        let (next_receive, next_change) = self.inner.next_indices();
        serde_wasm_bindgen::to_value(&dto::JsIndices { next_receive, next_change }).map_err(js)
    }

    /// `{ address, script_pubkey_hex }` at `.../<branch>/<index>` (branch 0 receive,
    /// 1 change) without touching the counters.
    #[wasm_bindgen(js_name = addressAt)]
    pub fn address_at(&self, branch: u32, index: u32) -> Result<JsValue, JsError> {
        let a = self.inner.address_at(branch, index).map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsAddress {
            script_pubkey_hex: hex::encode(a.script_pubkey().as_bytes()),
            address: a.to_string(),
        })
        .map_err(js)
    }

    /// Check that `address` parses and is valid on this view's network, returning
    /// its canonical string form. Cheap (no derivation) — validate a send
    /// recipient with this before any network I/O so a bad address reports a
    /// clear error instead of being masked by a later "no coins" check.
    #[wasm_bindgen(js_name = checkAddress)]
    pub fn check_address(&self, address: &str) -> Result<String, JsError> {
        Ok(parse_address(address, params(&self.chain)?.network)?.to_string())
    }

    /// `{ address, script_pubkey_hex }` for the next unused external address.
    #[wasm_bindgen(js_name = nextReceiveAddress)]
    pub fn next_receive_address(&mut self) -> Result<JsValue, JsError> {
        let a = self.inner.next_receive_address().map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsAddress {
            script_pubkey_hex: hex::encode(a.script_pubkey().as_bytes()),
            address: a.to_string(),
        })
        .map_err(js)
    }

    #[wasm_bindgen(js_name = nextChangeAddress)]
    pub fn next_change_address(&mut self) -> Result<JsValue, JsError> {
        let a = self.inner.next_change_address().map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsAddress {
            script_pubkey_hex: hex::encode(a.script_pubkey().as_bytes()),
            address: a.to_string(),
        })
        .map_err(js)
    }

    /// Coin-select and build the unsigned HTLC funding transaction.
    ///
    /// `utxos`: `[{ txid, vout, value_sat, script_pubkey_hex, confirmations,
    /// derivation_index, is_change }]`. Returns `{ tx_hex, fee_sat, change_sat|null,
    /// selected: [...] }` — pass `selected` straight to `Wallet.signFundingTx`.
    #[wasm_bindgen(js_name = planHtlcFunding)]
    pub fn plan_htlc_funding(
        &mut self,
        utxos: JsValue,
        htlc_script_pubkey_hex: &str,
        htlc_value_sat: u64,
        feerate_sat_vb: u64,
        min_confirmations: u32,
    ) -> Result<JsValue, JsError> {
        let js_utxos: Vec<dto::JsUtxo> = serde_wasm_bindgen::from_value(utxos).map_err(js)?;
        let utxos: Vec<_> =
            js_utxos.iter().map(|u| u.to_core()).collect::<Result<_, _>>().map_err(js)?;
        let out = TxOut {
            value: Amount::from_sat(htlc_value_sat),
            script_pubkey: spk(htlc_script_pubkey_hex)?,
        };
        let plan = self
            .inner
            .plan_payment(&utxos, vec![out], feerate_sat_vb, min_confirmations, None, false)
            .map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsFundingPlan::from_core(&plan)).map_err(js)
    }

    /// Coin-select and build an unsigned payment. `outputs` is
    /// `[{ address, amount_sat }]`. `opReturnHex` (optional) appends a 0-value
    /// `OP_RETURN` carrying those bytes — pass ~100 random bytes on the Bitcoin
    /// chain to make the tx consensus-invalid on the BLAKE2b fork (replay
    /// protection). `serviceFee` (optional, from the backend's `/v1/status`)
    /// appends the hosted backend's fee output. `feeFromAmount`: carve the
    /// network + service fee out of the amount (recipient gets `amount − fees`)
    /// instead of adding them on top. Returns `{ tx_hex, fee_sat, change_sat|null,
    /// service_fee_sat|null, selected }`.
    #[wasm_bindgen(js_name = planPayment)]
    #[allow(clippy::too_many_arguments)]
    pub fn plan_payment(
        &mut self,
        utxos: JsValue,
        outputs: JsValue,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        op_return_hex: Option<String>,
        service_fee: JsValue,
        fee_from_amount: bool,
    ) -> Result<JsValue, JsError> {
        let js_utxos: Vec<dto::JsUtxo> = serde_wasm_bindgen::from_value(utxos).map_err(js)?;
        let utxos: Vec<_> =
            js_utxos.iter().map(|u| u.to_core()).collect::<Result<_, _>>().map_err(js)?;
        let net = params(&self.chain)?.network;
        let js_outs: Vec<dto::JsPayTo> = serde_wasm_bindgen::from_value(outputs).map_err(js)?;
        let mut outs: Vec<TxOut> = js_outs
            .iter()
            .map(|o| {
                Ok(TxOut {
                    value: Amount::from_sat(o.amount_sat),
                    script_pubkey: address_spk(&o.address, net)?,
                })
            })
            .collect::<Result<_, JsError>>()?;
        if let Some(h) = op_return_hex.filter(|h| !h.is_empty()) {
            outs.push(wallet_core::op_return_output(&hex::decode(h).map_err(js)?).map_err(js)?);
        }
        let sf = parse_service_fee(&service_fee, net)?;
        let plan = self
            .inner
            .plan_payment(&utxos, outs, feerate_sat_vb, min_confirmations, sf.as_ref(), fee_from_amount)
            .map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsFundingPlan::from_core(&plan)).map_err(js)
    }

    /// Send the whole confirmed balance to `destAddress` (fee deducted, no change).
    /// `serviceFee` (optional) carves the hosted backend's fee out of the amount sent.
    #[wasm_bindgen(js_name = planSweep)]
    pub fn plan_sweep(
        &self,
        utxos: JsValue,
        dest_address: &str,
        feerate_sat_vb: u64,
        min_confirmations: u32,
        service_fee: JsValue,
    ) -> Result<JsValue, JsError> {
        let js_utxos: Vec<dto::JsUtxo> = serde_wasm_bindgen::from_value(utxos).map_err(js)?;
        let utxos: Vec<_> =
            js_utxos.iter().map(|u| u.to_core()).collect::<Result<_, _>>().map_err(js)?;
        let net = params(&self.chain)?.network;
        let dest = address_spk(dest_address, net)?;
        let sf = parse_service_fee(&service_fee, net)?;
        let plan = self
            .inner
            .plan_sweep(&utxos, dest, feerate_sat_vb, min_confirmations, sf.as_ref())
            .map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsFundingPlan::from_core(&plan)).map_err(js)
    }
}

fn parse_address(addr: &str, net: wallet_core::bitcoin::Network) -> Result<Address, JsError> {
    // Trim first: a stray newline/space makes rust-bitcoin report a misleading
    // "base58 error" for an otherwise-valid bech32 address.
    let addr = addr.trim();
    addr.parse::<Address<NetworkUnchecked>>()
        .map_err(|_| JsError::new(&format!("\"{addr}\" is not a valid address")))?
        .require_network(net)
        .map_err(|_| JsError::new(&format!("address {addr} is not valid on this network")))
}

fn address_spk(addr: &str, net: wallet_core::bitcoin::Network) -> Result<ScriptBuf, JsError> {
    Ok(parse_address(addr, net)?.script_pubkey())
}

/// `serviceFee` is `undefined`/`null` (self-hosted backend, no fee) or
/// `{ address, bps, floor_sat, cap_sat }` from the backend's `/v1/status`.
fn parse_service_fee(
    v: &JsValue,
    net: wallet_core::bitcoin::Network,
) -> Result<Option<wallet_core::ServiceFee>, JsError> {
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let sf: dto::JsServiceFee = serde_wasm_bindgen::from_value(v.clone()).map_err(js)?;
    Ok(Some(wallet_core::ServiceFee {
        bps: sf.bps,
        floor_sat: sf.floor_sat,
        cap_sat: sf.cap_sat,
        fee_spk: address_spk(&sf.address, net)?,
    }))
}

/// One HTLC leg. Build it from the agreed swap parameters, then use it to make and
/// finalize redeem / refund transactions.
#[wasm_bindgen]
pub struct Htlc {
    inner: HtlcContract,
}

#[wasm_bindgen]
impl Htlc {
    /// `redeem_pubkey` (hex) spends via the preimage; `refund_pubkey` (hex) via the
    /// timelock after `locktime` (unix seconds).
    pub fn build(
        chain: &str,
        hashlock_hex: &str,
        redeem_pubkey_hex: &str,
        refund_pubkey_hex: &str,
        locktime: u32,
        value_sat: u64,
    ) -> Result<Htlc, JsError> {
        let inner = HtlcContract::build(
            &params(chain)?,
            sha256::Hash::from_byte_array(bytes32(hashlock_hex)?),
            &pubkey(redeem_pubkey_hex)?,
            &pubkey(refund_pubkey_hex)?,
            locktime,
            Amount::from_sat(value_sat),
        );
        Ok(Htlc { inner })
    }

    #[wasm_bindgen(getter, js_name = scriptPubkeyHex)]
    pub fn script_pubkey_hex(&self) -> String {
        hex::encode(self.inner.script_pubkey.as_bytes())
    }

    #[wasm_bindgen(getter, js_name = witnessScriptHex)]
    pub fn witness_script_hex(&self) -> String {
        hex::encode(self.inner.witness_script.as_bytes())
    }

    /// Locate this contract's output in a counterparty funding tx. Returns
    /// `{ vout, value_sat }`, or throws if there is no matching output funded to at
    /// least `min_value_sat`.
    #[wasm_bindgen(js_name = findOutput)]
    pub fn find_output(&self, funding_tx_hex: &str, min_value_sat: u64) -> Result<JsValue, JsError> {
        let tx = tx_from_hex(funding_tx_hex)?;
        let (vout, value) = self
            .inner
            .find_output(&tx, Amount::from_sat(min_value_sat))
            .ok_or_else(|| JsError::new("no matching HTLC output in funding tx"))?;
        serde_wasm_bindgen::to_value(&dto::JsFoundOutput { vout, value_sat: value.to_sat() })
            .map_err(js)
    }

    /// Unsigned redeem tx (hashlock path) spending the HTLC to `payout_spk_hex`.
    #[wasm_bindgen(js_name = redeemTx)]
    pub fn redeem_tx(
        &self,
        prevout_txid: &str,
        prevout_vout: u32,
        payout_spk_hex: &str,
        fee_sat: u64,
    ) -> Result<String, JsError> {
        let tx = self
            .inner
            .redeem_tx(self.outpoint(prevout_txid, prevout_vout)?, spk(payout_spk_hex)?, Amount::from_sat(fee_sat))
            .map_err(js)?;
        Ok(tx_to_hex(&tx))
    }

    /// Unsigned refund tx (timelock path). `nLockTime` is set to the contract CLTV.
    #[wasm_bindgen(js_name = refundTx)]
    pub fn refund_tx(
        &self,
        prevout_txid: &str,
        prevout_vout: u32,
        payout_spk_hex: &str,
        fee_sat: u64,
    ) -> Result<String, JsError> {
        let tx = self
            .inner
            .refund_tx(self.outpoint(prevout_txid, prevout_vout)?, spk(payout_spk_hex)?, Amount::from_sat(fee_sat))
            .map_err(js)?;
        Ok(tx_to_hex(&tx))
    }

    /// 32-byte sighash (hex) for input `input_index` of `tx_hex` — hand to
    /// `Wallet.signSwap`.
    #[wasm_bindgen(js_name = spendSighash)]
    pub fn spend_sighash(&self, tx_hex: &str, input_index: usize) -> Result<String, JsError> {
        let tx = tx_from_hex(tx_hex)?;
        Ok(hex::encode(self.inner.spend_sighash(&tx, input_index).map_err(js)?))
    }

    #[wasm_bindgen(js_name = finalizeRedeem)]
    pub fn finalize_redeem(
        &self,
        tx_hex: &str,
        input_index: usize,
        sig_hex: &str,
        redeem_pubkey_hex: &str,
        preimage_hex: &str,
    ) -> Result<String, JsError> {
        let mut tx = tx_from_hex(tx_hex)?;
        let sig = hex::decode(sig_hex).map_err(js)?;
        self.inner.finalize_redeem(
            &mut tx,
            input_index,
            &sig,
            &pubkey(redeem_pubkey_hex)?,
            &bytes32(preimage_hex)?,
        );
        Ok(tx_to_hex(&tx))
    }

    #[wasm_bindgen(js_name = finalizeRefund)]
    pub fn finalize_refund(
        &self,
        tx_hex: &str,
        input_index: usize,
        sig_hex: &str,
        refund_pubkey_hex: &str,
    ) -> Result<String, JsError> {
        let mut tx = tx_from_hex(tx_hex)?;
        let sig = hex::decode(sig_hex).map_err(js)?;
        self.inner.finalize_refund(&mut tx, input_index, &sig, &pubkey(refund_pubkey_hex)?);
        Ok(tx_to_hex(&tx))
    }
}

impl Htlc {
    fn outpoint(&self, txid: &str, vout: u32) -> Result<OutPoint, JsError> {
        Ok(OutPoint { txid: txid.parse::<Txid>().map_err(js)?, vout })
    }
}

// ---------------------------------------------------------------------------
// Seed sealing
// ---------------------------------------------------------------------------

fn kek32(kek: &[u8]) -> Result<[u8; 32], JsError> {
    kek.try_into().map_err(|_| JsError::new("kek must be 32 bytes"))
}
fn nonce24(nonce: &[u8]) -> Result<[u8; 24], JsError> {
    nonce.try_into().map_err(|_| JsError::new("nonce must be 24 bytes"))
}

/// Seal a mnemonic under a 32-byte key (e.g. a WebAuthn-PRF secret). `nonce` must be
/// 24 fresh random bytes. Returns a hex blob for IndexedDB / Keychain / Keystore.
#[wasm_bindgen(js_name = sealMnemonic)]
pub fn seal_mnemonic(mnemonic: &str, kek: &[u8], nonce: &[u8]) -> Result<String, JsError> {
    Ok(hex::encode(
        crypto::seal(mnemonic.as_bytes(), &kek32(kek)?, &nonce24(nonce)?).map_err(js)?,
    ))
}

#[wasm_bindgen(js_name = unsealMnemonic)]
pub fn unseal_mnemonic(blob_hex: &str, kek: &[u8]) -> Result<String, JsError> {
    let pt = crypto::unseal(&hex::decode(blob_hex).map_err(js)?, &kek32(kek)?).map_err(js)?;
    String::from_utf8(pt.to_vec()).map_err(|_| JsError::new("sealed data is not a mnemonic"))
}

/// Password path: Argon2id stretches `password` (with `salt`) to a KEK inside wasm —
/// the KEK never reaches JS. `salt` (>= 8 bytes) is stored beside the blob.
#[wasm_bindgen(js_name = sealMnemonicWithPassword)]
pub fn seal_mnemonic_with_password(
    mnemonic: &str,
    password: &str,
    salt: &[u8],
    nonce: &[u8],
) -> Result<String, JsError> {
    let kek = crypto::kek_from_password(password.as_bytes(), salt, &KdfParams::default()).map_err(js)?;
    Ok(hex::encode(
        crypto::seal(mnemonic.as_bytes(), &kek, &nonce24(nonce)?).map_err(js)?,
    ))
}

#[wasm_bindgen(js_name = unsealMnemonicWithPassword)]
pub fn unseal_mnemonic_with_password(
    blob_hex: &str,
    password: &str,
    salt: &[u8],
) -> Result<String, JsError> {
    let kek = crypto::kek_from_password(password.as_bytes(), salt, &KdfParams::default()).map_err(js)?;
    let pt = crypto::unseal(&hex::decode(blob_hex).map_err(js)?, &kek).map_err(js)?;
    String::from_utf8(pt.to_vec()).map_err(|_| JsError::new("wrong password or corrupt data"))
}

// ---------------------------------------------------------------------------
// SwapSession — the client half of one atomic swap
// ---------------------------------------------------------------------------

fn chain_enum(s: &str) -> Result<Chain, JsError> {
    match s {
        "btc" => Ok(Chain::Btc),
        "xbt" => Ok(Chain::Xbt),
        _ => Err(JsError::new("chain must be \"btc\" or \"xbt\"")),
    }
}

fn state_tag(s: &SwapState) -> String {
    match s {
        SwapState::Negotiated => "negotiated",
        SwapState::OurFundingBroadcast { .. } => "ourFundingBroadcast",
        SwapState::CounterpartyFundingConfirmed => "counterpartyFundingConfirmed",
        SwapState::Redeemed { .. } => "redeemed",
        SwapState::Settled => "settled",
        SwapState::Refunded => "refunded",
        SwapState::Failed { .. } => "failed",
    }
    .to_string()
}

fn to_swap_params(j: dto::JsSwapParams) -> Result<SwapParams, JsError> {
    let swap_id: [u8; 16] = hex::decode(&j.swap_id_hex)
        .map_err(js)?
        .try_into()
        .map_err(|_| JsError::new("swap_id_hex must be 16 bytes"))?;
    let role = match j.role.as_str() {
        "initiator" => SwapRole::Initiator,
        "participant" => SwapRole::Participant,
        _ => return Err(JsError::new("role must be \"initiator\" or \"participant\"")),
    };
    Ok(SwapParams {
        swap_id,
        role,
        send_chain: chain_enum(&j.send_chain)?,
        recv_chain: chain_enum(&j.recv_chain)?,
        send_amount: Amount::from_sat(j.send_amount_sat),
        recv_amount: Amount::from_sat(j.recv_amount_sat),
        hashlock: sha256::Hash::from_byte_array(bytes32(&j.hashlock_hex)?),
        our_pubkey_send: pubkey(&j.our_pubkey_send_hex)?,
        their_pubkey_send: pubkey(&j.their_pubkey_send_hex)?,
        our_pubkey_recv: pubkey(&j.our_pubkey_recv_hex)?,
        their_pubkey_recv: pubkey(&j.their_pubkey_recv_hex)?,
        our_contract_locktime: j.our_contract_locktime,
        their_contract_locktime: j.their_contract_locktime,
        required_incoming_confs: j.required_incoming_confs,
    })
}

fn spend_json(s: &UnsignedSpend) -> Result<JsValue, JsError> {
    serde_wasm_bindgen::to_value(&dto::JsUnsignedSpend {
        tx_hex: tx_to_hex(&s.tx),
        sighash_hex: hex::encode(s.sighash),
    })
    .map_err(js)
}

/// Tracks one swap: builds the HTLC legs, verifies the counterparty's funding,
/// produces unsigned redeem/refund transactions + their sighashes, and advances a
/// small state machine on chain events. Holds no keys.
#[wasm_bindgen]
pub struct SwapSession {
    inner: SwapMachine,
    send: ChainParams,
    recv: ChainParams,
}

#[wasm_bindgen]
impl SwapSession {
    /// `params` is a `JsSwapParams` object (see the README).
    #[wasm_bindgen(constructor)]
    pub fn new(params: JsValue) -> Result<SwapSession, JsError> {
        let j: dto::JsSwapParams = serde_wasm_bindgen::from_value(params).map_err(js)?;
        if !j.network.is_empty() {
            set_network(&j.network)?;
        }
        let send = self::params(&j.send_chain)?;
        let recv = self::params(&j.recv_chain)?;
        Ok(SwapSession { inner: SwapMachine::new(to_swap_params(j)?), send, recv })
    }

    #[wasm_bindgen(getter)]
    pub fn state(&self) -> String {
        state_tag(self.inner.state())
    }

    #[wasm_bindgen(getter)]
    pub fn role(&self) -> String {
        match self.inner.role() {
            SwapRole::Initiator => "initiator".into(),
            SwapRole::Participant => "participant".into(),
        }
    }

    /// The preimage, hex, once it is public on-chain (state `redeemed`).
    #[wasm_bindgen(getter, js_name = revealedPreimage)]
    pub fn revealed_preimage(&self) -> Option<String> {
        match self.inner.state() {
            SwapState::Redeemed { preimage } => Some(hex::encode(preimage)),
            _ => None,
        }
    }

    /// Our HTLC on the send chain: `{ address, script_pubkey_hex, witness_script_hex,
    /// locktime, value_sat }`. Fund `address` with the agreed amount.
    #[wasm_bindgen(js_name = ourContract)]
    pub fn our_contract(&mut self) -> Result<JsValue, JsError> {
        let net = self.send.network;
        let c = self.inner.build_our_contract(&self.send).map_err(js)?;
        let address = wallet_core::bitcoin::Address::from_script(&c.script_pubkey, net)
            .map_err(js)?
            .to_string();
        serde_wasm_bindgen::to_value(&serde_json::json!({
            "address": address,
            "script_pubkey_hex": hex::encode(c.script_pubkey.as_bytes()),
            "witness_script_hex": hex::encode(c.witness_script.as_bytes()),
            "locktime": c.locktime,
            "value_sat": c.value.to_sat(),
        }))
        .map_err(js)
    }

    /// Verify the counterparty's funding tx contains our expected HTLC output.
    /// Returns `{ txid, vout }`; throws on any mismatch or short funding.
    #[wasm_bindgen(js_name = verifyTheirFunding)]
    pub fn verify_their_funding(&mut self, funding_tx_hex: &str) -> Result<JsValue, JsError> {
        let raw = hex::decode(funding_tx_hex).map_err(js)?;
        let op = self.inner.verify_their_contract(&raw, &self.recv).map_err(js)?;
        serde_wasm_bindgen::to_value(&dto::JsOutPoint { txid: op.txid.to_string(), vout: op.vout })
            .map_err(js)
    }

    /// Unsigned refund of our own contract. `{ tx_hex, sighash_hex }` — sign
    /// `sighash_hex` with `Wallet.signSwap`, then `finalizeRefund`.
    #[wasm_bindgen(js_name = buildRefund)]
    pub fn build_refund(
        &self,
        our_contract_txid: &str,
        vout: u32,
        payout_spk_hex: &str,
        fee_sat: u64,
    ) -> Result<JsValue, JsError> {
        let op = OutPoint { txid: our_contract_txid.parse().map_err(js)?, vout };
        spend_json(&self.inner.build_refund(op, spk(payout_spk_hex)?, Amount::from_sat(fee_sat)).map_err(js)?)
    }

    /// Unsigned redeem of the counterparty's contract using the preimage.
    #[wasm_bindgen(js_name = buildRedeem)]
    pub fn build_redeem(
        &self,
        their_contract_txid: &str,
        vout: u32,
        payout_spk_hex: &str,
        fee_sat: u64,
    ) -> Result<JsValue, JsError> {
        let op = OutPoint { txid: their_contract_txid.parse().map_err(js)?, vout };
        spend_json(&self.inner.build_redeem(op, spk(payout_spk_hex)?, Amount::from_sat(fee_sat)).map_err(js)?)
    }

    #[wasm_bindgen(js_name = finalizeRefund)]
    pub fn finalize_refund(
        &self,
        tx_hex: &str,
        sig_hex: &str,
        refund_pubkey_hex: &str,
    ) -> Result<String, JsError> {
        let mut tx = tx_from_hex(tx_hex)?;
        self.inner
            .finalize_our_refund(&mut tx, &hex::decode(sig_hex).map_err(js)?, &pubkey(refund_pubkey_hex)?)
            .map_err(js)?;
        Ok(tx_to_hex(&tx))
    }

    #[wasm_bindgen(js_name = finalizeRedeem)]
    pub fn finalize_redeem(
        &self,
        tx_hex: &str,
        sig_hex: &str,
        redeem_pubkey_hex: &str,
        preimage_hex: &str,
    ) -> Result<String, JsError> {
        let mut tx = tx_from_hex(tx_hex)?;
        self.inner
            .finalize_their_redeem(
                &mut tx,
                &hex::decode(sig_hex).map_err(js)?,
                &pubkey(redeem_pubkey_hex)?,
                &bytes32(preimage_hex)?,
            )
            .map_err(js)?;
        Ok(tx_to_hex(&tx))
    }

    #[wasm_bindgen(js_name = onOurFundingBroadcast)]
    pub fn on_our_funding_broadcast(&mut self, txid: &str) -> Result<String, JsError> {
        let txid: Txid = txid.parse().map_err(js)?;
        Ok(state_tag(self.inner.on_event(SwapEvent::OurFundingBroadcast { txid })))
    }

    #[wasm_bindgen(js_name = onCounterpartyFunded)]
    pub fn on_counterparty_funded(&mut self, confirmations: u32) -> String {
        state_tag(self.inner.on_event(SwapEvent::CounterpartyFunded { confirmations }))
    }

    #[wasm_bindgen(js_name = onCounterpartyRedeemed)]
    pub fn on_counterparty_redeemed(&mut self, preimage_hex: &str) -> Result<String, JsError> {
        Ok(state_tag(
            self.inner.on_event(SwapEvent::CounterpartyRedeemed { preimage: bytes32(preimage_hex)? }),
        ))
    }

    #[wasm_bindgen(js_name = onOurOutputConfirmed)]
    pub fn on_our_output_confirmed(&mut self, confirmations: u32) -> String {
        state_tag(self.inner.on_event(SwapEvent::OurOutputConfirmed { confirmations }))
    }

    #[wasm_bindgen(js_name = onTimelockExpired)]
    pub fn on_timelock_expired(&mut self) -> String {
        state_tag(self.inner.on_event(SwapEvent::TimelockExpired))
    }
}
