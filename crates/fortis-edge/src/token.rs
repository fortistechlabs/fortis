//! Stateless per-install tokens: `<id>.<tag>` where `tag = HMAC-SHA256(secret,
//! id)` truncated to 16 bytes. Any instance holding the same secret can verify a
//! token with no shared storage, which is what keeps the edge horizontally
//! scalable. Revocation would need a blocklist — not built yet.

use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const ID_LEN: usize = 16;
const TAG_LEN: usize = 16;

/// Load the signing secret from `path`, creating 32 random bytes if absent.
pub fn load_or_create_secret(path: &std::path::Path) -> Result<Vec<u8>> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 16 {
            return Ok(bytes);
        }
    }
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).map_err(|e| anyhow!("CSPRNG: {e}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, secret).with_context(|| format!("writing {}", path.display()))?;
    Ok(secret.to_vec())
}

fn tag(secret: &[u8], id: &[u8]) -> [u8; TAG_LEN] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(id);
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&full[..TAG_LEN]);
    out
}

/// Mint a fresh token.
pub fn issue(secret: &[u8]) -> Result<String> {
    let mut id = [0u8; ID_LEN];
    getrandom::getrandom(&mut id).map_err(|e| anyhow!("CSPRNG: {e}"))?;
    Ok(format!("{}.{}", hex::encode(id), hex::encode(tag(secret, &id))))
}

/// Constant-time check that `token` was minted with `secret`.
pub fn verify(secret: &[u8], token: &str) -> bool {
    let Some((id_hex, tag_hex)) = token.split_once('.') else {
        return false;
    };
    let (Ok(id), Ok(got)) = (hex::decode(id_hex), hex::decode(tag_hex)) else {
        return false;
    };
    if id.len() != ID_LEN || got.len() != TAG_LEN {
        return false;
    }
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&id);
    // verify_truncated_left is constant-time.
    mac.verify_truncated_left(&got).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_tokens_verify_and_tampered_ones_do_not() {
        let secret = b"a-test-secret-key-32-bytes-long!!";
        let t = issue(secret).unwrap();
        assert!(verify(secret, &t));

        assert!(!verify(b"a-different-secret-of-good-length", &t));

        let (id, tag) = t.split_once('.').unwrap();
        let last = tag.chars().last().unwrap();
        let repl = if last == '0' { '1' } else { '0' };
        let flipped = format!("{id}.{}{repl}", &tag[..tag.len() - 1]);
        assert!(!verify(secret, &flipped));

        assert!(!verify(secret, "garbage"));
        assert!(!verify(secret, "not.hex"));
        assert!(!verify(secret, ""));
    }

    #[test]
    fn secret_is_persisted_and_reused() {
        let dir = std::env::temp_dir().join(format!("fe-tok-{}", std::process::id()));
        let path = dir.join("secret");
        let _ = std::fs::remove_dir_all(&dir);
        let a = load_or_create_secret(&path).unwrap();
        let b = load_or_create_secret(&path).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
