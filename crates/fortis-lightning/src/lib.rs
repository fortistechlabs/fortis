//! fortis Lightning — the only crate that touches LDK.
//!
//! Wraps [`ldk_node`] (0.7 line, LDK v0.2) with a VSS backend
//! ([`vss_client`], the `vss-client-ng` release ldk-node itself uses). The
//! public API (`Node`, `parse`, `LnError`, …) is filled in by later tasks; see
//! `docs/superpowers/specs/2026-10-30-lightning-design.md`.

pub use ldk_node;
pub use vss_client;

mod channel;
mod device;
mod error;
pub mod lnurl;
mod node;
mod parse;
mod pay;
mod receive;
pub mod recover;

pub use channel::ChannelState;
pub use error::{LnError, ParseError};
#[doc(hidden)]
pub use node::test_hooks;
pub use node::{
    vss_state_exists, Balances, ChainSource, LnEvent, LspInfo, LspLimits, Node, NodeConfig,
    StartMode, VssConfig,
};
pub use parse::{parse, parse_at, PaymentTarget};
pub use pay::{Direction, Payment, Status};
pub use receive::ReceiveQuote;
