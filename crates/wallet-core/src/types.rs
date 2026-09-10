//! Small shared aliases. Most types come straight from `bitcoin`.

/// 32-byte HTLC preimage (the swap secret).
pub type Preimage = [u8; 32];
