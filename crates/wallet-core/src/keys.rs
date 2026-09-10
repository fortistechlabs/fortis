//! HD key management (BIP-32/39/84). Holds secrets; nothing leaves the core except
//! xpubs and per-swap public keys.

use bip39::Mnemonic;
use bitcoin::bip32::{DerivationPath, Xpriv, Xpub};
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1};
use bitcoin::{CompressedPublicKey, NetworkKind, ScriptBuf, Transaction, TxOut, Witness};
use zeroize::Zeroizing;

use crate::chain::ChainParams;
use crate::error::{Result, WalletError};
use crate::sighash::sighash_all;

/// The wallet's master secret, derived from a BIP-39 mnemonic.
pub struct MasterKey {
    xpriv: Xpriv,
}

/// The 2048-word BIP-39 English wordlist, in standard order. Frontends use it to
/// offer word completions while the user types a recovery phrase.
pub fn bip39_wordlist() -> &'static [&'static str; 2048] {
    bip39::Language::English.word_list()
}

impl MasterKey {
    /// Build a fresh 24-word mnemonic from 256 bits of platform entropy, plus the
    /// derived key. The caller shows the words to the user for backup.
    pub fn generate(entropy: &[u8; 32]) -> Result<(Mnemonic, MasterKey)> {
        let mnemonic = Mnemonic::from_entropy(entropy).map_err(|_| WalletError::InvalidMnemonic)?;
        let key = Self::from_mnemonic(&mnemonic, "")?;
        Ok((mnemonic, key))
    }

    /// Like [`generate`](Self::generate) but for `words` (12 or 24) and folding
    /// shell-collected `extra` entropy (pointer/touch jitter, device-motion
    /// noise, dice, …) into the CSPRNG bytes first — see [`crate::entropy`].
    /// `csprng` must still be ≥ 32 fresh bytes from the OS RNG; `extra`
    /// supplements it, never replaces it.
    pub fn generate_mixed(
        csprng: &[u8],
        extra: &[&[u8]],
        words: u8,
    ) -> Result<(Mnemonic, MasterKey)> {
        let entropy_bytes = match words {
            12 => 16usize,
            24 => 32usize,
            _ => return Err(WalletError::InvalidMnemonic),
        };
        if csprng.len() < 32 {
            return Err(WalletError::InvalidMnemonic);
        }
        let mut sources: Vec<&[u8]> = Vec::with_capacity(extra.len() + 1);
        sources.push(csprng);
        sources.extend_from_slice(extra);
        let mixed = crate::entropy::mix_entropy(&sources);
        let mnemonic = Mnemonic::from_entropy(&mixed[..entropy_bytes])
            .map_err(|_| WalletError::InvalidMnemonic)?;
        let key = Self::from_mnemonic(&mnemonic, "")?;
        Ok((mnemonic, key))
    }

    pub fn from_mnemonic(mnemonic: &Mnemonic, passphrase: &str) -> Result<MasterKey> {
        let seed = Zeroizing::new(mnemonic.to_seed(passphrase));
        let xpriv = Xpriv::new_master(NetworkKind::Main, seed.as_ref())
            .map_err(|e| WalletError::Derivation(e.to_string()))?;
        Ok(MasterKey { xpriv })
    }

    pub fn from_phrase(phrase: &str, passphrase: &str) -> Result<MasterKey> {
        let mnemonic = Mnemonic::parse(phrase).map_err(|_| WalletError::InvalidMnemonic)?;
        Self::from_mnemonic(&mnemonic, passphrase)
    }

    /// BIP-32 fingerprint of the master key — the `[abcd1234/…]` key-origin prefix
    /// a descriptor wallet wants when importing this account's xpub watch-only.
    pub fn master_fingerprint(&self) -> bitcoin::bip32::Fingerprint {
        self.xpriv.fingerprint(&Secp256k1::new())
    }

    /// Account xpub at `m/84'/<coin_type>'/<account>'`, for a [`crate::wallet::WalletView`].
    ///
    /// The serialization version follows `params.network`: `xpub…` on mainnet,
    /// `tpub…` on regtest — a regtest node rejects a mainnet-versioned key in a
    /// descriptor.
    pub fn account_xpub(&self, params: &ChainParams, account: u32) -> Result<Xpub> {
        let secp = Secp256k1::new();
        let child = self.xpriv.derive_priv(&secp, &account_path(params.bip44_coin_type, account)?)?;
        let mut xpub = Xpub::from_priv(&secp, &child);
        xpub.network = NetworkKind::from(params.network);
        Ok(xpub)
    }

    /// Ephemeral keypair for one swap, under a hardened branch so a leaked swap key
    /// can never reach wallet funds: `m/84'/<coin>'/<account>'/2'/<swap_index>'`.
    fn swap_xpriv(&self, params: &ChainParams, account: u32, swap_index: u32) -> Result<Xpriv> {
        let secp = Secp256k1::new();
        let path = swap_path(params.bip44_coin_type, account, swap_index)?;
        Ok(self.xpriv.derive_priv(&secp, &path)?)
    }

    pub fn swap_pubkey(&self, params: &ChainParams, account: u32, swap_index: u32) -> Result<PublicKey> {
        let secp = Secp256k1::new();
        Ok(self.swap_xpriv(params, account, swap_index)?.private_key.public_key(&secp))
    }

    /// ECDSA-sign a 32-byte sighash message with the swap key. Returns the DER
    /// signature (low-s) with the sighash flag byte appended — `0x01` on Bitcoin,
    /// `0x21` (`ALL | UNIFIED`) on the BLAKE2b chain — ready for `Htlc::finalize_*`.
    pub fn sign_swap(
        &self,
        params: &ChainParams,
        account: u32,
        swap_index: u32,
        msg: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let secp = Secp256k1::new();
        let sk = self.swap_xpriv(params, account, swap_index)?.private_key;
        let mut out = secp.sign_ecdsa(&Message::from_digest(*msg), &sk).serialize_der().to_vec();
        out.push(sighash_flag(params));
        Ok(out)
    }

    /// Public key for a wallet (P2WPKH) address at `m/84'/<coin>'/<account>'/<branch>/<index>`,
    /// where `branch` is 1 for change, 0 for receive.
    pub fn wallet_pubkey(
        &self,
        params: &ChainParams,
        account: u32,
        is_change: bool,
        index: u32,
    ) -> Result<PublicKey> {
        let secp = Secp256k1::new();
        Ok(self.wallet_xpriv(params, account, is_change, index)?.private_key.public_key(&secp))
    }

    fn wallet_xpriv(
        &self,
        params: &ChainParams,
        account: u32,
        is_change: bool,
        index: u32,
    ) -> Result<Xpriv> {
        let branch = u32::from(is_change);
        let path = parse_path(&format!(
            "m/84h/{}h/{account}h/{branch}/{index}",
            params.bip44_coin_type
        ))?;
        Ok(self.xpriv.derive_priv(&Secp256k1::new(), &path)?)
    }

    /// Sign every input of a P2WPKH-funded transaction in place (`SIGHASH_ALL`, or
    /// `ALL | UNIFIED` on the BLAKE2b chain). `prevouts[i]` is the output
    /// `tx.input[i]` spends; `paths[i]` is `(is_change, derivation_index)` for that
    /// input's key.
    pub fn sign_p2wpkh_tx(
        &self,
        params: &ChainParams,
        account: u32,
        tx: &mut Transaction,
        prevouts: &[TxOut],
        paths: &[(bool, u32)],
    ) -> Result<()> {
        if prevouts.len() != tx.input.len() || paths.len() != tx.input.len() {
            return Err(WalletError::InvalidSwapParams(
                "prevouts / paths length must match tx inputs".into(),
            ));
        }
        let secp = Secp256k1::new();
        let variant = params.sighash_variant();
        let mut witnesses = Vec::with_capacity(tx.input.len());
        for (i, &(is_change, index)) in paths.iter().enumerate() {
            let sk = self.wallet_xpriv(params, account, is_change, index)?.private_key;
            let pk = sk.public_key(&secp);
            // The prevout this input spends must be the P2WPKH output of the key we
            // just derived — otherwise `paths` was mispaired with the inputs and
            // the signature would be worthless (a silently un-broadcastable tx).
            let expected_spk =
                ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash());
            if prevouts[i].script_pubkey != expected_spk {
                return Err(WalletError::InvalidSwapParams(format!(
                    "input {i}: prevout scriptPubKey does not match the key at (change={is_change}, index={index})"
                )));
            }
            // BIP-143 scriptCode for a P2WPKH input is the implied P2PKH script.
            let script_code = ScriptBuf::new_p2pkh(&bitcoin::PublicKey::new(pk).pubkey_hash());
            let sh = sighash_all(&*tx, prevouts, i, &script_code, variant)?;
            let mut sig = secp
                .sign_ecdsa(&Message::from_digest(sh.message), &sk)
                .serialize_der()
                .to_vec();
            sig.push(sh.flag);
            let mut w = Witness::new();
            w.push(&sig);
            w.push(pk.serialize());
            witnesses.push(w);
        }
        for (input, witness) in tx.input.iter_mut().zip(witnesses) {
            input.witness = witness;
        }
        Ok(())
    }
}

fn sighash_flag(params: &ChainParams) -> u8 {
    if params.require_unified_sighash {
        0x21
    } else {
        0x01
    }
}

impl From<bitcoin::bip32::Error> for WalletError {
    fn from(e: bitcoin::bip32::Error) -> Self {
        WalletError::Derivation(e.to_string())
    }
}

fn account_path(coin_type: u32, account: u32) -> Result<DerivationPath> {
    parse_path(&format!("m/84h/{coin_type}h/{account}h"))
}

fn swap_path(coin_type: u32, account: u32, swap_index: u32) -> Result<DerivationPath> {
    parse_path(&format!("m/84h/{coin_type}h/{account}h/2h/{swap_index}h"))
}

fn parse_path(s: &str) -> Result<DerivationPath> {
    s.parse()
        .map_err(|e: bitcoin::bip32::Error| WalletError::Derivation(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::ChainParams;
    use bitcoin::hashes::Hash;
    use bitcoin::Txid;

    // BIP-39 vector: 16 bytes of zeros -> the canonical 12-word "abandon..." phrase.
    #[test]
    fn bip39_zero_entropy_vector() {
        let m = Mnemonic::from_entropy(&[0u8; 16]).unwrap();
        assert_eq!(
            m.to_string(),
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
        );
    }

    #[test]
    fn generate_mixed_folds_in_extra_and_rejects_short_csprng() {
        let csprng = [3u8; 32];
        let (plain, _) = MasterKey::generate_mixed(&csprng, &[], 24).unwrap();
        let (with_dice, _) = MasterKey::generate_mixed(&csprng, &[b"6 1 4 4 2 5 3 1"], 24).unwrap();
        assert_ne!(plain.to_string(), with_dice.to_string());
        assert_eq!(plain.to_string().split(' ').count(), 24);

        // same inputs -> same phrase
        let (again, _) = MasterKey::generate_mixed(&csprng, &[b"6 1 4 4 2 5 3 1"], 24).unwrap();
        assert_eq!(with_dice.to_string(), again.to_string());

        assert!(MasterKey::generate_mixed(&[0u8; 16], &[], 24).is_err());
    }

    #[test]
    fn generate_mixed_word_count() {
        let csprng = [7u8; 32];
        assert_eq!(MasterKey::generate_mixed(&csprng, &[], 12).unwrap().0.to_string().split(' ').count(), 12);
        assert_eq!(MasterKey::generate_mixed(&csprng, &[], 24).unwrap().0.to_string().split(' ').count(), 24);
        assert!(MasterKey::generate_mixed(&csprng, &[], 18).is_err());
        // 12 and 24 from the same csprng share the first 128 bits but differ as phrases
        assert_ne!(
            MasterKey::generate_mixed(&csprng, &[], 12).unwrap().0.to_string(),
            MasterKey::generate_mixed(&csprng, &[], 24).unwrap().0.to_string(),
        );
    }

    // BIP-32 test vector 1: seed 000102...0f, chain m/0H extended private key.
    #[test]
    fn bip32_vector_1_m_0h() {
        let seed = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let master = Xpriv::new_master(NetworkKind::Main, &seed).unwrap();
        let secp = Secp256k1::new();
        let child = master
            .derive_priv(&secp, &parse_path("m/0h").unwrap())
            .unwrap();
        assert_eq!(
            child.to_string(),
            "xprv9uHRZZhk6KAJC1avXpDAp4MDc3sQKNxDiPvvkX8Br5ngLNv1TxvUxt4cV1rGL5hj6KCesnDYUhd7oWgT11eZG7XnxHrnYeSvkzY7d2bhkJ7"
        );
    }

    #[test]
    fn swap_keys_are_deterministic_and_distinct() {
        let (_m, key) = MasterKey::generate(&[7u8; 32]).unwrap();
        let p = ChainParams::bitcoin();
        let a = key.swap_pubkey(&p, 0, 0).unwrap();
        let a2 = key.swap_pubkey(&p, 0, 0).unwrap();
        let b = key.swap_pubkey(&p, 0, 1).unwrap();
        assert_eq!(a, a2);
        assert_ne!(a, b);
    }

    #[test]
    fn signs_p2wpkh_funding_input() {
        use bitcoin::{
            absolute, transaction, Amount, CompressedPublicKey, OutPoint, ScriptBuf, Sequence,
            Transaction, TxIn, TxOut, Witness,
        };

        let (_m, key) = MasterKey::generate(&[5u8; 32]).unwrap();
        let p = ChainParams::bitcoin();
        let pk = key.wallet_pubkey(&p, 0, false, 0).unwrap();
        let spk = ScriptBuf::new_p2wpkh(&CompressedPublicKey(pk).wpubkey_hash());

        let mut tx = Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::all_zeros(), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut { value: Amount::from_sat(90_000), script_pubkey: ScriptBuf::new() }],
        };
        let prevout = TxOut { value: Amount::from_sat(100_000), script_pubkey: spk };

        key.sign_p2wpkh_tx(&p, 0, &mut tx, &[prevout], &[(false, 0)]).unwrap();

        let w = &tx.input[0].witness;
        assert_eq!(w.len(), 2);
        assert_eq!(w.nth(1).unwrap().len(), 33); // pubkey
        assert_eq!(w.nth(0).unwrap().last(), Some(&0x01)); // SIGHASH_ALL byte
    }
}
