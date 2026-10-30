//! Moving funds onto Lightning and back: [`Node::transit_address`],
//! [`Node::open_channel`], [`Node::close_channel`],
//! [`Node::force_close_channel`], [`Node::sweep_transit`] and
//! [`Node::channel_state`].
//!
//! **Transit.** ldk-node's built-in on-chain wallet is only a transit
//! wallet: the BTC wallet sends to [`Node::transit_address`], a channel open
//! is funded from it, and a close pays back into it, from where
//! [`Node::sweep_transit`] sends everything to the BTC wallet.
//!
//! **Anchor reserve.** Channels to the LSP are anchor channels, so while one
//! exists ldk-node keeps `AnchorChannelsConfig::per_channel_reserve_sats`
//! (default 25 000 sat per channel) back in the transit wallet to fee-bump a
//! force close. `open_channel` therefore needs `amount + 25 000` spendable
//! (the funding fee comes out of that reserve), and `sweep_transit` leaves
//! the reserve in place while a channel exists. [`Balances`] reports the
//! spendable part only.
//!
//! **Gate.** Every call here that moves funds or changes channel state
//! passes the same pre-send check as `pay` (a VSS round trip, ≤ 3 s):
//! `BackupUnavailable` and nothing happens when VSS does not answer.
//!
//! [`Balances`]: crate::Balances

use std::str::FromStr;

use ldk_node::bitcoin::address::NetworkUnchecked;
use ldk_node::bitcoin::Address;
use ldk_node::lightning::chain::channelmonitor::BalanceSource;
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::{BalanceDetails, LightningBalance, NodeError, PendingSweepBalance};

use crate::{LnError, Node};

/// ldk-node's dust limit for its own wallet outputs.
const DUST_LIMIT_SAT: u64 = 546;

/// Where the node's channels to the LSP stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// No channel and nothing on its way back from one (also: not running).
    None,
    /// A channel was opened and is waiting for the funding transaction's
    /// confirmations (the LSP's `minimum_depth`).
    Opening,
    /// A channel is confirmed and open (it may be briefly unusable while the
    /// LSP is disconnected).
    Ready,
    /// A close is being negotiated, or a closed channel's funds are not yet
    /// spendable in the transit wallet (`Balances::pending_close_sat > 0`).
    Closing,
}

/// One channel in ldk-node's channel list, as [`derive_state`] needs it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChannelFact {
    /// `ChannelDetails::is_channel_ready`. LDK sets it from the channel's
    /// `is_usable()`: `channel_ready` exchanged **and no `shutdown` sent or
    /// received** (it does not depend on the peer being connected).
    pub ready: bool,
    /// The funding transaction has the confirmations the channel needs
    /// (`confirmations >= max(confirmations_required, 1)`).
    pub funding_confirmed: bool,
}

impl ChannelFact {
    fn of(c: &ldk_node::ChannelDetails) -> ChannelFact {
        ChannelFact::of_counts(
            c.is_channel_ready,
            c.confirmations_required,
            c.confirmations,
        )
    }

    fn of_counts(ready: bool, required: Option<u32>, confirmations: Option<u32>) -> ChannelFact {
        ChannelFact {
            ready,
            funding_confirmed: confirmations.unwrap_or(0) >= required.unwrap_or(1).max(1),
        }
    }

    /// Confirmed, but no longer ready: a `shutdown` was sent or received.
    fn shutting_down(&self) -> bool {
        !self.ready && self.funding_confirmed
    }
}

/// What [`derive_state`] decides from: no ldk-node types.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChannelFacts {
    /// Channels ldk-node still lists (not yet closed).
    pub channels: Vec<ChannelFact>,
    /// [`split_lightning`]'s pending-close amount.
    pub pending_close_sat: u64,
}

/// `Ready` beats `Opening` beats `Closing` beats `None`: the state the user
/// can act on. A listed channel that is not ready is opening until its
/// funding has the needed confirmations, and shutting down (closing) after:
/// LDK clears `is_channel_ready` once `shutdown` is exchanged. (A channel
/// shut down before its funding confirmed therefore reads as `Opening` until
/// it disappears from the list.)
pub(crate) fn derive_state(f: &ChannelFacts) -> ChannelState {
    if f.channels.iter().any(|c| c.ready) {
        ChannelState::Ready
    } else if f.channels.iter().any(|c| !c.shutting_down()) {
        ChannelState::Opening
    } else if !f.channels.is_empty() || f.pending_close_sat > 0 {
        ChannelState::Closing
    } else {
        ChannelState::None
    }
}

/// One balance a channel monitor reports, as [`split_lightning`] needs it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorBalance {
    /// The channel is still in ldk-node's channel list.
    pub channel_open: bool,
    /// What LDK counts as ours (`Balance::claimable_amount_satoshis`).
    pub claimable_sat: u64,
    /// The output already pays the transit wallet and is confirmed there
    /// (a confirmed cooperative close), so the wallet balance counts it.
    pub in_wallet: bool,
}

/// `(lightning_sat, pending_close_sat)` from ldk-node's
/// `total_lightning_balance_sats` (the sum of every monitor's claimable
/// amounts), the per-monitor balances and the closing sweeps not yet
/// confirmed:
///
/// - `lightning_sat` is the total less the balances of channels no longer
///   open, i.e. what the open channels hold;
/// - `pending_close_sat` is what closed channels still hold on chain (the
///   to-self output under its timelock, HTLCs, a closing transaction not yet
///   confirmed) plus outputs ldk-node's sweeper is moving into the transit
///   wallet whose sweep is not yet confirmed. A confirmed cooperative close
///   pays the transit wallet directly and is left out (the wallet counts
///   it), as are sweeps already confirmed.
pub(crate) fn split_lightning(
    total_lightning_sat: u64,
    monitors: &[MonitorBalance],
    unconfirmed_sweeps_sat: u64,
) -> (u64, u64) {
    let closed: u64 = monitors
        .iter()
        .filter(|m| !m.channel_open)
        .map(|m| m.claimable_sat)
        .sum();
    let closed_pending: u64 = monitors
        .iter()
        .filter(|m| !m.channel_open && !m.in_wallet)
        .map(|m| m.claimable_sat)
        .sum();
    (
        total_lightning_sat.saturating_sub(closed),
        closed_pending + unconfirmed_sweeps_sat,
    )
}

/// [`split_lightning`] over ldk-node's balances and channel list.
pub(crate) fn lightning_balances(b: &BalanceDetails, open: &[ChannelId]) -> (u64, u64) {
    let monitors: Vec<MonitorBalance> = b
        .lightning_balances
        .iter()
        .map(|lb| monitor_balance(lb, open))
        .collect();
    let sweeps: u64 = b
        .pending_balances_from_channel_closures
        .iter()
        .map(|s| match s {
            PendingSweepBalance::PendingBroadcast {
                amount_satoshis, ..
            }
            | PendingSweepBalance::BroadcastAwaitingConfirmation {
                amount_satoshis, ..
            } => *amount_satoshis,
            PendingSweepBalance::AwaitingThresholdConfirmations { .. } => 0,
        })
        .sum();
    split_lightning(b.total_lightning_balance_sats, &monitors, sweeps)
}

fn monitor_balance(lb: &LightningBalance, open: &[ChannelId]) -> MonitorBalance {
    // Same amounts as LDK's `Balance::claimable_amount_satoshis`, which is
    // what `total_lightning_balance_sats` sums.
    let (channel_id, claimable_sat, in_wallet) = match lb {
        LightningBalance::ClaimableOnChannelClose {
            channel_id,
            amount_satoshis,
            ..
        }
        | LightningBalance::ContentiousClaimable {
            channel_id,
            amount_satoshis,
            ..
        }
        | LightningBalance::CounterpartyRevokedOutputClaimable {
            channel_id,
            amount_satoshis,
            ..
        } => (channel_id, *amount_satoshis, false),
        LightningBalance::ClaimableAwaitingConfirmations {
            channel_id,
            amount_satoshis,
            source,
            ..
        } => (
            channel_id,
            *amount_satoshis,
            *source == BalanceSource::CoopClose,
        ),
        LightningBalance::MaybeTimeoutClaimableHTLC {
            channel_id,
            amount_satoshis,
            outbound_payment,
            ..
        } => (
            channel_id,
            if *outbound_payment {
                0
            } else {
                *amount_satoshis
            },
            false,
        ),
        LightningBalance::MaybePreimageClaimableHTLC { channel_id, .. } => (channel_id, 0, false),
    };
    MonitorBalance {
        channel_open: open.contains(channel_id),
        claimable_sat,
        in_wallet,
    }
}

fn not_started() -> LnError {
    LnError::Other("not_started".into())
}

impl Node {
    /// A fresh address of the node's transit wallet (ldk-node's on-chain
    /// wallet), for the BTC wallet's "Move to Lightning" send.
    ///
    /// Errors: `Other("not_started")`; `BackupUnavailable` when the wallet
    /// cannot persist the new address index.
    pub fn transit_address(&self) -> Result<String, LnError> {
        let running = self.running().ok_or_else(not_started)?;
        running
            .node
            .onchain_payment()
            .new_address()
            .map(|a| a.to_string())
            .map_err(|e| match e {
                NodeError::NotRunning => not_started(),
                NodeError::PersistenceFailed => LnError::BackupUnavailable,
                other => LnError::Other(format!("transit_address: {other}")),
            })
    }

    /// Opens an unannounced channel of `amount_sat` to the LSP, funded from
    /// the transit wallet. Returns once the open is initiated (connect,
    /// `open_channel`, ldk-node funds and broadcasts it in the background):
    /// [`channel_state`](Self::channel_state) is `Opening` until the LSP's
    /// confirmations, then `Ready` (with `LnEvent::ChannelReady`). If the
    /// LSP rejects the open, it ends with `LnEvent::ChannelClosed`.
    ///
    /// Errors, in order: `AmountOutOfRange` (msat) unless
    /// `channel_min_sat <= amount_sat <= channel_max_sat`;
    /// `Other("not_started")`; `Other("insufficient_transit")` unless the
    /// spendable transit balance covers `amount_sat` plus the anchor reserve
    /// (module docs); `BackupUnavailable` from the gate; `LspUnreachable`.
    ///
    /// **Blocking.** The VSS check (≤ 3 s) and, when the LSP is not
    /// connected, a connection attempt: call it off the UI thread.
    pub fn open_channel(&self, amount_sat: u64) -> Result<(), LnError> {
        let limits = self.limits();
        if amount_sat < limits.channel_min_sat || amount_sat > limits.channel_max_sat {
            return Err(LnError::AmountOutOfRange {
                min_msat: limits.channel_min_sat.saturating_mul(1_000),
                max_msat: limits.channel_max_sat.saturating_mul(1_000),
            });
        }
        let running = self.running().ok_or_else(not_started)?;
        let node = &running.node;
        let reserve = node
            .config()
            .anchor_channels_config
            .map_or(0, |c| c.per_channel_reserve_sats);
        let spendable = node.list_balances().spendable_onchain_balance_sats;
        if spendable < amount_sat.saturating_add(reserve) {
            return Err(LnError::Other("insufficient_transit".into()));
        }
        self.pre_send_checks()?;
        let (lsp_id, lsp_addr) = self.lsp_peer();
        node.open_channel(lsp_id, lsp_addr, amount_sat, None, None)
            .map(|_user_channel_id| ())
            .map_err(|e| match e {
                NodeError::NotRunning => not_started(),
                NodeError::ConnectionFailed => LnError::LspUnreachable,
                NodeError::InsufficientFunds => LnError::Other("insufficient_transit".into()),
                NodeError::PersistenceFailed => LnError::BackupUnavailable,
                other => LnError::Other(format!("open_channel: {other}")),
            })
    }

    /// Cooperatively closes every channel with the LSP; the funds come back
    /// to the transit wallet once the closing transaction confirms
    /// (`channel_state` is `Closing` until then). `Ok` also when every
    /// channel is already shutting down. Never force-closes: see
    /// [`force_close_channel`](Self::force_close_channel).
    ///
    /// Errors: `Other("not_started")`; `Other("no_channel")`;
    /// `LspUnreachable` when the LSP cannot be reached (a cooperative close
    /// needs it online); `BackupUnavailable` from the gate;
    /// `Other("close_failed")` when LDK refuses (e.g. HTLCs in flight).
    pub fn close_channel(&self) -> Result<(), LnError> {
        let running = self.running().ok_or_else(not_started)?;
        let node = &running.node;
        let (lsp_id, lsp_addr) = self.lsp_peer();
        let channels = node.list_channels();
        if channels.is_empty() {
            return Err(LnError::Other("no_channel".into()));
        }
        if !peer_connected(node, &lsp_id) && node.connect(lsp_id, lsp_addr.clone(), true).is_err() {
            return Err(LnError::LspUnreachable);
        }
        self.pre_send_checks()?;
        let mut result = Ok(());
        for c in &channels {
            // Already shutting down (see `derive_state`): nothing to start.
            if ChannelFact::of(c).shutting_down() {
                continue;
            }
            if let Err(e) = node.close_channel(&c.user_channel_id, c.counterparty_node_id) {
                let err = if !peer_connected(node, &c.counterparty_node_id) {
                    LnError::LspUnreachable
                } else if e == NodeError::PersistenceFailed {
                    LnError::BackupUnavailable
                } else {
                    LnError::Other("close_failed".into())
                };
                result = result.and(Err(err));
            }
        }
        self.keep_lsp_peer(node);
        result
    }

    /// Force-closes every channel with the LSP, broadcasting the latest
    /// commitment. **Only on an explicit user action** (the LSP is gone for
    /// good): the funds are timelocked (the LSP's `to_self_delay`) before
    /// ldk-node sweeps them into the transit wallet, and on-chain fees are
    /// higher. Prefer [`close_channel`](Self::close_channel).
    ///
    /// Errors: `Other("not_started")`; `Other("no_channel")`;
    /// `BackupUnavailable` from the gate; `Other("force_close_failed")`.
    pub fn force_close_channel(&self) -> Result<(), LnError> {
        let running = self.running().ok_or_else(not_started)?;
        let node = &running.node;
        let channels = node.list_channels();
        if channels.is_empty() {
            return Err(LnError::Other("no_channel".into()));
        }
        self.pre_send_checks()?;
        let mut result = Ok(());
        for c in &channels {
            if let Err(e) = node.force_close_channel(
                &c.user_channel_id,
                c.counterparty_node_id,
                Some("closed by the user".into()),
            ) {
                let err = if e == NodeError::PersistenceFailed {
                    LnError::BackupUnavailable
                } else {
                    LnError::Other("force_close_failed".into())
                };
                result = result.and(Err(err));
            }
        }
        self.keep_lsp_peer(node);
        result
    }

    /// Sends everything spendable in the transit wallet to `to_address` (the
    /// BTC wallet's next receive address), keeping the anchor reserve back
    /// only while a channel exists. `Ok(Some(txid))` once broadcast;
    /// `Ok(None)` when the spendable transit balance is at most the dust
    /// limit (546 sat). Unconfirmed incoming transit funds are not spent.
    ///
    /// Errors, in order: `Other("bad_address")` (not an address);
    /// `WrongNetwork` (not for the node's network); `Other("not_started")`;
    /// `BackupUnavailable` from the gate (nothing sent);
    /// `Other("sweep: …")` when ldk-node cannot build it (e.g. the balance
    /// does not cover the fee).
    pub fn sweep_transit(&self, to_address: &str) -> Result<Option<String>, LnError> {
        let address = Address::<NetworkUnchecked>::from_str(to_address)
            .map_err(|_| LnError::Other("bad_address".into()))?
            .require_network(self.network())
            .map_err(|_| LnError::WrongNetwork)?;
        let running = self.running().ok_or_else(not_started)?;
        let node = &running.node;
        if node.list_balances().spendable_onchain_balance_sats <= DUST_LIMIT_SAT {
            return Ok(None);
        }
        let retain_reserves = self.channel_state() != ChannelState::None;
        self.pre_send_checks()?;
        match node
            .onchain_payment()
            .send_all_to_address(&address, retain_reserves, None)
        {
            Ok(txid) => Ok(Some(txid.to_string())),
            // ldk-node: nothing left once the reserve and the fee are kept.
            Err(NodeError::InsufficientFunds) => Ok(None),
            Err(NodeError::NotRunning) => Err(not_started()),
            Err(NodeError::InvalidAddress) => Err(LnError::WrongNetwork),
            Err(NodeError::PersistenceFailed) => Err(LnError::BackupUnavailable),
            Err(other) => Err(LnError::Other(format!("sweep: {other}"))),
        }
    }

    /// See [`ChannelState`]; `None` when the node is not running. Derived
    /// from ldk-node's channel list (readiness, funding confirmations) and
    /// its channel-monitor and sweeper balances, all of which ldk-node
    /// persists, so it holds across restarts (a shutdown still being
    /// negotiated resumes when the LSP reconnects).
    pub fn channel_state(&self) -> ChannelState {
        let Some(running) = self.running() else {
            return ChannelState::None;
        };
        let node = &running.node;
        let channels = node.list_channels();
        let open: Vec<ChannelId> = channels.iter().map(|c| c.channel_id).collect();
        let (_, pending_close_sat) = lightning_balances(&node.list_balances(), &open);
        derive_state(&ChannelFacts {
            channels: channels.iter().map(ChannelFact::of).collect(),
            pending_close_sat,
        })
    }

    /// ldk-node forgets a peer (no reconnect while running) when its last
    /// channel is closed; the LSP stays a persisted peer for receives. Only
    /// when connected (then `connect` just re-persists, no network wait);
    /// otherwise the next `start` persists it again.
    fn keep_lsp_peer(&self, node: &ldk_node::Node) {
        let (lsp_id, lsp_addr) = self.lsp_peer();
        if peer_connected(node, &lsp_id) {
            let _ = node.connect(lsp_id, lsp_addr, true);
        }
    }
}

fn peer_connected(node: &ldk_node::Node, id: &ldk_node::bitcoin::secp256k1::PublicKey) -> bool {
    node.list_peers()
        .iter()
        .any(|p| p.node_id == *id && p.is_connected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(ready: bool, funding_confirmed: bool) -> ChannelFact {
        ChannelFact {
            ready,
            funding_confirmed,
        }
    }

    fn state(channels: &[ChannelFact], pending_close_sat: u64) -> ChannelState {
        derive_state(&ChannelFacts {
            channels: channels.to_vec(),
            pending_close_sat,
        })
    }

    #[test]
    fn channel_state_derivation() {
        use ChannelState as S;
        // Nothing at all.
        assert_eq!(state(&[], 0), S::None);
        // Funding short of its confirmations.
        assert_eq!(state(&[ch(false, false)], 0), S::Opening);
        // Ready (whether or not the LSP is connected right now).
        assert_eq!(state(&[ch(true, true)], 0), S::Ready);
        // Confirmed but no longer ready: shutdown exchanged.
        assert_eq!(state(&[ch(false, true)], 0), S::Closing);
        // Closed, funds still on their way to transit.
        assert_eq!(state(&[], 149_056), S::Closing);
        // Ready wins over everything; Opening over Closing.
        assert_eq!(state(&[ch(false, true), ch(true, true)], 5), S::Ready);
        assert_eq!(state(&[ch(false, false)], 149_056), S::Opening);
        assert_eq!(state(&[ch(false, true), ch(false, false)], 0), S::Opening);
    }

    #[test]
    fn channel_fact_from_confirmations() {
        let fact = |required, confs| ChannelFact::of_counts(false, required, confs);
        assert!(!fact(Some(6), Some(5)).funding_confirmed);
        assert!(fact(Some(6), Some(6)).funding_confirmed);
        // Zero-conf (JIT) channels: unconfirmed funding is not "confirmed".
        assert!(!fact(Some(0), Some(0)).funding_confirmed);
        assert!(fact(Some(0), Some(1)).funding_confirmed);
        assert!(!fact(None, None).funding_confirmed);
    }

    #[test]
    fn lightning_split_moves_closed_channels_to_pending() {
        let open = |sat| MonitorBalance {
            channel_open: true,
            claimable_sat: sat,
            in_wallet: false,
        };
        let closed = |sat, in_wallet| MonitorBalance {
            channel_open: false,
            claimable_sat: sat,
            in_wallet,
        };
        // Only open channels: all of it is Lightning.
        assert_eq!(split_lightning(149_056, &[open(149_056)], 0), (149_056, 0));
        // A force-closed channel awaiting its timelock, plus a sweep in flight.
        assert_eq!(
            split_lightning(200_000, &[open(100_000), closed(100_000, false)], 7_000),
            (100_000, 107_000)
        );
        // A confirmed cooperative close is already in the wallet.
        assert_eq!(
            split_lightning(149_000, &[closed(149_000, true)], 0),
            (0, 0)
        );
        // Never underflows.
        assert_eq!(split_lightning(0, &[closed(10, false)], 0), (0, 10));
    }
}
