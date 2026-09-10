//! Watch-only chain gateway for fortis.
//!
//! A small blocking JSON-RPC client for a Bitcoin Core / Knots node ([`Rpc`]) plus
//! the wallet operations a shell needs on top of it — chain status, a watch-only
//! descriptor wallet, UTXO reads, fee estimation, history, broadcast. **No keys**:
//! coin selection and signing happen in `wallet-core` on the client side; this
//! crate only ever sees public data and finished transactions.

mod ops;
mod rpc;

pub use ops::{
    broadcast, chain_status, collect_utxos, descriptor_checksum, ensure_watch_wallet,
    estimate_feerate, history, import_account, import_descriptors, import_request, scanning,
    test_accept, wallet_balances, wpkh_branch_descriptor, Balances, ChainStatus, HistoryEntry,
};
pub use rpc::{cookie_auth, Rpc};
