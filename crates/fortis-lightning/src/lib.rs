//! fortis Lightning — the only crate that touches LDK.
//!
//! Wraps [`ldk_node`] (0.7 line, LDK v0.2) with a VSS backend
//! ([`vss_client`], the `vss-client-ng` release ldk-node itself uses). The
//! public API (`Node`, `parse`, `LnError`, …) is filled in by later tasks; see
//! `docs/superpowers/specs/2026-10-30-lightning-design.md`.

pub use ldk_node;
pub use vss_client;
