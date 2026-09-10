//! Sealing the seed / mnemonic at rest.
//!
//! The key-encryption key (KEK) comes from the platform: a WebAuthn-PRF secret on
//! web, Secure Enclave / StrongBox on mobile. When only a password is available,
//! [`kek_from_password`] stretches it with Argon2id first. The sealed blob then
//! lives in IndexedDB / Keychain / Keystore via the [`crate::storage`] traits.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::error::{Result, WalletError};

/// AEAD overhead: 24-byte XNonce prefix + 16-byte Poly1305 tag.
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Argon2id cost parameters. Defaults follow the OWASP minimum for interactive use.
#[derive(Debug, Clone, Copy)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self { m_cost_kib: 19_456, t_cost: 2, p_cost: 1 }
    }
}

/// Seal `plaintext` under `kek` with XChaCha20-Poly1305. Output is
/// `nonce (24) || ciphertext || tag`.
///
/// `nonce` MUST be 24 fresh random bytes from a CSPRNG for every call — reuse with
/// the same key is catastrophic.
pub fn seal(plaintext: &[u8], kek: &[u8; 32], nonce: &[u8; NONCE_LEN]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(kek));
    let ct = cipher
        .encrypt(XNonce::from_slice(nonce), plaintext)
        .map_err(|_| WalletError::Crypto("seal failed".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Reverse [`seal`]. Fails on a wrong key or any tampering (AEAD).
pub fn unseal(blob: &[u8], kek: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(WalletError::Crypto("sealed blob too short".into()));
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(kek));
    let pt = cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| WalletError::Crypto("unseal failed — wrong key or corrupt data".into()))?;
    Ok(Zeroizing::new(pt))
}

/// Stretch a low-entropy password into a 32-byte KEK with Argon2id. `salt` must be
/// at least 8 bytes and stored alongside the blob.
pub fn kek_from_password(
    password: &[u8],
    salt: &[u8],
    params: &KdfParams,
) -> Result<Zeroizing<[u8; 32]>> {
    let p = Params::new(params.m_cost_kib, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| WalletError::Crypto(format!("argon2 params: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
    let mut out = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password, salt, out.as_mut())
        .map_err(|e| WalletError::Crypto(format!("argon2: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_unseal_roundtrip() {
        let kek = [1u8; 32];
        let nonce = [2u8; 24];
        let blob = seal(b"correct horse battery staple", &kek, &nonce).unwrap();
        assert_eq!(&blob[..24], &nonce);
        assert_eq!(&*unseal(&blob, &kek).unwrap(), b"correct horse battery staple");
    }

    #[test]
    fn unseal_rejects_wrong_key_and_tampering() {
        let blob = seal(b"secret", &[1u8; 32], &[9u8; 24]).unwrap();
        assert!(unseal(&blob, &[0u8; 32]).is_err());

        let mut bad = blob.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(unseal(&bad, &[1u8; 32]).is_err());

        assert!(unseal(&blob[..30], &[1u8; 32]).is_err());
    }

    #[test]
    fn argon2_is_deterministic_and_salt_sensitive() {
        let fast = KdfParams { m_cost_kib: 256, t_cost: 1, p_cost: 1 };
        let a = kek_from_password(b"pw", b"salt-one", &fast).unwrap();
        let a2 = kek_from_password(b"pw", b"salt-one", &fast).unwrap();
        let b = kek_from_password(b"pw", b"salt-two", &fast).unwrap();
        assert_eq!(*a, *a2);
        assert_ne!(*a, *b);
    }

    #[test]
    fn password_kek_seals_a_mnemonic() {
        let fast = KdfParams { m_cost_kib: 256, t_cost: 1, p_cost: 1 };
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        let kek = kek_from_password(b"hunter2", b"0123456789abcdef", &fast).unwrap();
        let blob = seal(phrase.as_bytes(), &kek, &[7u8; 24]).unwrap();
        let back = unseal(&blob, &kek).unwrap();
        assert_eq!(std::str::from_utf8(&back).unwrap(), phrase);
    }
}
