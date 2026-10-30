//! Error enums. The `Display` strings are stable lowercase keys that the
//! Kotlin layer maps to user-facing text; do not reword them.

use thiserror::Error;

/// Why a scanned/pasted string could not be turned into a [`crate::PaymentTarget`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("empty")]
    Empty,
    #[error("unrecognised")]
    Unrecognised,
    #[error("wrong_network")]
    WrongNetwork,
    #[error("expired")]
    Expired,
    #[error("invalid: {0}")]
    Invalid(String),
}

/// Errors surfaced by the Lightning node API.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LnError {
    #[error("backup_unavailable")]
    BackupUnavailable,
    #[error("active_on_another_device")]
    ActiveOnAnotherDevice,
    #[error("lsp_unreachable")]
    LspUnreachable,
    #[error("insufficient_lightning")]
    InsufficientLightning,
    #[error("no_route")]
    NoRoute,
    #[error("invoice_expired")]
    InvoiceExpired,
    #[error("wrong_network")]
    WrongNetwork,
    #[error("amount_out_of_range")]
    AmountOutOfRange { min_msat: u64, max_msat: u64 },
    #[error("other: {0}")]
    Other(String),
}
