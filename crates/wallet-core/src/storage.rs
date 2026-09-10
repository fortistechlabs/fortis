//! Capability traits the platform shell implements — the core performs no I/O.

/// Hardware-backed secure storage: iOS Keychain / Secure Enclave, Android Keystore /
/// StrongBox, or a WebAuthn-PRF-derived key over IndexedDB on the web.
pub trait SecureStorage {
    fn seal(&self, key: &str, plaintext: &[u8]) -> core::result::Result<(), StorageError>;
    fn unseal(&self, key: &str) -> core::result::Result<Option<Vec<u8>>, StorageError>;
    fn delete(&self, key: &str) -> core::result::Result<(), StorageError>;
}

/// Platform CSPRNG (`getrandom` on native, `crypto.getRandomValues` on web).
pub trait Entropy {
    fn fill(&self, buf: &mut [u8]);
}

#[derive(Debug)]
pub struct StorageError(pub String);

impl core::fmt::Display for StorageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "secure storage: {}", self.0)
    }
}

impl std::error::Error for StorageError {}
