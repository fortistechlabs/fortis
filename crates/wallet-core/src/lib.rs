//! # wallet-core
//!
//! Shared client-side wallet logic for BTC2Blake, compiled to WASM for the web app
//! and to a native library (via UniFFI) for the iOS / Android apps. The
//! security-critical code lives here once and is audited once.
//!
//! **Non-custodial.** This crate derives keys, builds and signs transactions, and
//! runs the client half of the HTLC atomic-swap protocol. It performs **no I/O** —
//! the platform shell supplies data (UTXOs, confirmations, the counterparty's
//! contract) and carries out storage, networking, and biometrics through the traits
//! in [`storage`].
//!
//! ## Status
//!
//! `keys`, `htlc`, `sighash` (BIP-143 + `SIGHASH_UNIFIED`, Knots PR #357), `swap`,
//! `crypto` seed sealing, and `wallet` (address derivation, coin selection, sweeps)
//! are implemented and tested. Remaining: the mobile (`wallet-ffi`) bindings.

#![forbid(unsafe_code)]

pub mod chain;
pub mod crypto;
pub mod entropy;
pub mod error;
pub mod htlc;
pub mod keys;
pub mod sighash;
pub mod storage;
pub mod swap;
pub mod types;
pub mod wallet;

pub use bitcoin;

pub use chain::{Chain, ChainParams};
pub use entropy::mix_entropy;
pub use error::{Result, WalletError};
pub use htlc::HtlcContract;
pub use keys::{bip39_wordlist, MasterKey};
pub use swap::{SwapEvent, SwapMachine, SwapParams, SwapRole, SwapState};
pub use wallet::{op_return_output, FundingPlan, ServiceFee, Utxo, WalletView};
