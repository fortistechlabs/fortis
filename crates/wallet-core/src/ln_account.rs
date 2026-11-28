//! The wallet's Lightning node identity and Fortis account request signing.
//!
//! The Fortis account (the LSP's ledger, keyed by node id) takes requests signed with the
//! node key `ldk-node` would have for the same seed, so the web wallet and the phone share
//! one account. No Lightning library is linked: the derivation is BIP-85 (HEX, 64 bytes,
//! index 0 = BTC, 1 = XBT), then LDK's `KeysManager` (seeded with the BIP-32 master private
//! key of those bytes, node secret = `m/0'` of a master built from that seed). The signature
//! is LN message signing (zbase32) over [`ACCOUNT_SIG_PREFIX`] + body, as `fortis-lsp` checks.
//! `fortis-lightning/tests/web_account_identity.rs` pins both against LDK itself.

use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use bitcoin::Network;
use zeroize::Zeroizing;

use crate::keys::{bip85_hex_from_master, MasterKey};

/// Prepended to a request body before signing (`fortis-lsp` `account_auth::SIG_PREFIX`).
pub const ACCOUNT_SIG_PREFIX: &str = "fortis-account-v1:";

const LN_MESSAGE_PREFIX: &[u8] = b"Lightning Signed Message:";
const ZBASE32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

/// Which chain's Lightning identity (each chain has its own node seed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LnChainId {
    Btc,
    Xbt,
}

impl LnChainId {
    fn seed_index(self) -> u32 {
        match self {
            LnChainId::Btc => 0,
            LnChainId::Xbt => 1,
        }
    }
}

fn node_secret(master: &Xpriv, chain: LnChainId) -> Zeroizing<[u8; 32]> {
    let node_seed = bip85_hex_from_master(master, 64, chain.seed_index());
    // ldk-node: KeysManager seed = BIP-32 master private key of the 64 seed bytes.
    let mut km_master =
        Xpriv::new_master(Network::Testnet, &node_seed).expect("64 bytes make a master key");
    let km_seed = Zeroizing::new(km_master.private_key.secret_bytes());
    km_master.private_key.non_secure_erase();
    // KeysManager::new: node secret = m/0' of a master built from that seed.
    let mut m = Xpriv::new_master(Network::Testnet, &km_seed[..]).expect("32 bytes make a master key");
    let mut child = m
        .derive_priv(&Secp256k1::new(), &[ChildNumber::Hardened { index: 0 }])
        .expect("hardened derivation cannot fail");
    m.private_key.non_secure_erase();
    let out = Zeroizing::new(child.private_key.secret_bytes());
    child.private_key.non_secure_erase();
    out
}

fn zbase32(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ZBASE32[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ZBASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

impl MasterKey {
    /// The Lightning node id (33-byte compressed public key, hex) for `chain`.
    pub fn ln_node_id(&self, chain: LnChainId) -> String {
        let secret = node_secret(self.master_xpriv(), chain);
        let sk = SecretKey::from_slice(&secret[..]).expect("derived key is valid");
        PublicKey::from_secret_key(&Secp256k1::new(), &sk).to_string()
    }

    /// LN message signature (zbase32) of [`ACCOUNT_SIG_PREFIX`] + `body` with `chain`'s node key.
    pub fn ln_sign_account(&self, chain: LnChainId, body: &str) -> String {
        let secret = node_secret(self.master_xpriv(), chain);
        let sk = SecretKey::from_slice(&secret[..]).expect("derived key is valid");
        let msg = [LN_MESSAGE_PREFIX, ACCOUNT_SIG_PREFIX.as_bytes(), body.as_bytes()].concat();
        let digest = sha256d::Hash::hash(&msg);
        let sig = Secp256k1::signing_only()
            .sign_ecdsa_recoverable(&Message::from_digest(digest.to_byte_array()), &sk);
        let (rid, compact) = sig.serialize_compact();
        let mut raw = Vec::with_capacity(65);
        raw.push(rid.to_i32() as u8 + 31);
        raw.extend_from_slice(&compact);
        zbase32(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    /// Pinned against ldk-node's own derivation by `fortis-lightning/tests/web_account_identity.rs`.
    #[test]
    fn node_ids_are_pinned() {
        let k = MasterKey::from_phrase(PHRASE, "").unwrap();
        assert_eq!(k.ln_node_id(LnChainId::Btc), "038244e129da7c5e62de4a34437903945413d7b5a134643070afb0dc35474ffb41");
        assert_eq!(k.ln_node_id(LnChainId::Xbt), "0309072b38c3750ede9e3b866a5887c309fe9bf49cefdb7f7786a0a1e4473dc1f8");
    }
}
