use thiserror::Error;

pub type Result<T> = core::result::Result<T, WalletError>;

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("invalid mnemonic")]
    InvalidMnemonic,

    #[error("key derivation failed: {0}")]
    Derivation(String),

    #[error("insufficient funds: need {need} sat, have {have} sat")]
    InsufficientFunds { need: u64, have: u64 },

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("invalid swap parameters: {0}")]
    InvalidSwapParams(String),

    #[error("swap state error: expected {expected}, got {actual}")]
    SwapState { expected: String, actual: String },

    #[error("counterparty contract does not match agreed parameters")]
    ContractMismatch,

    #[error("storage: {0}")]
    Storage(String),

    #[error("crypto: {0}")]
    Crypto(String),

    #[error("bitcoin: {0}")]
    Bitcoin(String),

    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}
