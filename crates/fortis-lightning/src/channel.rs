//! Moving funds onto Lightning and back: [`Node::transit_address`],
//! [`Node::open_channel`], [`Node::close_channel`],
//! [`Node::force_close_channel`], [`Node::sweep_transit`] and
//! [`Node::channel_state`]; and the split of the node's funds into the
//! [`Balances`] fields.
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
//! (the funding fee is paid out of what is above `amount`), and
//! `sweep_transit` leaves the reserve in place while a channel exists.
//!
//! **Serialised.** `open_channel`, `close_channel`, `force_close_channel`
//! and `sweep_transit` run one at a time per [`Node`] (none of them waits
//! for confirmations while holding that lock).
//!
//! **Gate.** Each of those passes the same pre-send check as `pay` (a VSS
//! round trip, ≤ 3 s): `BackupUnavailable` and nothing happens when VSS does
//! not answer.
//!
//! **Close records.** ldk-node 0.7.1 does not expose a channel's shutdown
//! state, and LDK reports a channel whose funding confirmed before the
//! LSP's `channel_ready` arrived exactly like one that is shutting down
//! (`is_channel_ready == false`). The node therefore records, per channel
//! id, in `<data_dir>/fortis-channel-closes.json` (atomic write + fsync):
//! `coop` when `close_channel` asks LDK to close it (written before the
//! call, removed again if LDK refuses), `force` for `force_close_channel`,
//! and the kind of every `ChannelClosed` event `next_event` sees. A record
//! is dropped once the channel is neither listed nor holds any balance.
//! Only a record is evidence of a shutdown: an unrecorded not-ready channel
//! is `Opening`.
//!
//! [`Balances`]: crate::Balances

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use ldk_node::bitcoin::address::NetworkUnchecked;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::bitcoin::Address;
use ldk_node::lightning::chain::channelmonitor::BalanceSource;
use ldk_node::lightning::events::ClosureReason;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::ln::types::ChannelId;
use ldk_node::{ChannelDetails, LightningBalance, NodeError, PendingSweepBalance};

use crate::{Balances, LnError, Node};

/// ldk-node's dust limit for its own wallet outputs.
const DUST_LIMIT_SAT: u64 = 546;
const CLOSES_FILE: &str = "fortis-channel-closes.json";
static CLOSES_LOCK: Mutex<()> = Mutex::new(());

/// Where the node's channels to the LSP stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// No channel and nothing on its way back from one (also: not running).
    None,
    /// A channel was opened and is not yet usable: its funding is short of
    /// the LSP's confirmations, or the LSP's `channel_ready` has not arrived.
    Opening,
    /// A channel is confirmed and open (it may be briefly unusable while the
    /// LSP is disconnected).
    Ready,
    /// A close was asked for and is being negotiated, or a closed channel
    /// still has funds or claims to resolve on chain.
    Closing,
}

/// How a channel was (or is being) closed, from the close records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseKind {
    Coop,
    Force,
}

impl CloseKind {
    fn as_str(self) -> &'static str {
        match self {
            CloseKind::Coop => "coop",
            CloseKind::Force => "force",
        }
    }

    fn parse(s: &str) -> Option<CloseKind> {
        match s {
            "coop" => Some(CloseKind::Coop),
            "force" => Some(CloseKind::Force),
            _ => None,
        }
    }

    fn of_reason(reason: Option<&ClosureReason>) -> CloseKind {
        match reason {
            Some(
                ClosureReason::LegacyCooperativeClosure
                | ClosureReason::CounterpartyInitiatedCooperativeClosure
                | ClosureReason::LocallyInitiatedCooperativeClosure
                | ClosureReason::CounterpartyCoopClosedUnfundedChannel
                | ClosureReason::LocallyCoopClosedUnfundedChannel,
            ) => CloseKind::Coop,
            _ => CloseKind::Force,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure model: no ldk-node types below until `impl Node`.
// ---------------------------------------------------------------------------

/// One channel in ldk-node's channel list, as [`derive_state`] needs it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ChannelFact {
    /// `ChannelDetails::is_channel_ready` (LDK: `channel_ready` exchanged
    /// and no `shutdown` sent or received).
    pub ready: bool,
    /// A close record exists for it: positive evidence of a shutdown.
    pub close_recorded: bool,
}

/// What [`derive_state`] decides from.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChannelFacts {
    /// Channels ldk-node still lists (not yet closed).
    pub channels: Vec<ChannelFact>,
    /// [`closing_unresolved`] or a non-zero pending-close amount.
    pub closed_funds_pending: bool,
}

/// `Ready` beats `Opening` beats `Closing` beats `None`: the state the user
/// can act on. A listed channel that is not ready is `Opening` unless it has
/// a close record.
pub(crate) fn derive_state(f: &ChannelFacts) -> ChannelState {
    if f.channels.iter().any(|c| c.ready) {
        ChannelState::Ready
    } else if f.channels.iter().any(|c| !c.close_recorded) {
        ChannelState::Opening
    } else if !f.channels.is_empty() || f.closed_funds_pending {
        ChannelState::Closing
    } else {
        ChannelState::None
    }
}

/// What a channel-monitor balance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BalanceKind {
    /// `ClaimableOnChannelClose`: no closing transaction confirmed yet.
    OnClose,
    /// `ClaimableAwaitingConfirmations` from a cooperative close: the
    /// output pays the transit wallet and is confirmed there.
    CoopConfirmed,
    /// Anything else: a force-close output under its timelock or awaiting
    /// confirmations, HTLC claims, contentious or revoked-output claims.
    Resolving,
}

/// One balance a channel monitor reports.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorBalance {
    /// The channel is still in ldk-node's channel list.
    pub channel_open: bool,
    /// What LDK counts as ours (`Balance::claimable_amount_satoshis`).
    pub claimable_sat: u64,
    pub kind: BalanceKind,
    /// The channel's close record.
    pub close: Option<CloseKind>,
}

/// Where one of ldk-node's sweeper outputs is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepState {
    /// No sweep transaction broadcast yet.
    PendingBroadcast,
    /// Sweep broadcast, unconfirmed; `in_wallet` when the transit wallet
    /// already counts its output.
    Broadcast { in_wallet: bool },
    /// Sweep confirmed: the transit wallet counts it.
    Confirmed,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SweepBalance {
    pub amount_sat: u64,
    pub state: SweepState,
}

/// ldk-node's on-chain wallet as `list_balances` reports it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WalletBalance {
    /// `total_onchain_balance_sats`: everything, unconfirmed included.
    pub total_sat: u64,
    /// `spendable_onchain_balance_sats`: confirmed + own unconfirmed change,
    /// less the anchor reserve.
    pub spendable_sat: u64,
    /// `total_anchor_channels_reserve_sats`: `min(reserve, total)`.
    pub reserve_sat: u64,
}

/// Everything [`breakdown`] and [`closing_unresolved`] need.
#[derive(Debug, Clone, Default)]
pub(crate) struct BalanceInputs {
    /// `total_lightning_balance_sats`: every monitor's claimable amounts.
    pub total_lightning_sat: u64,
    pub monitors: Vec<MonitorBalance>,
    pub sweeps: Vec<SweepBalance>,
    pub wallet: WalletBalance,
}

/// While a closed channel still has anything to resolve on chain other than
/// a cooperative close (whose output simply pays the wallet), or a sweep of
/// a closed channel's output is not yet confirmed. A force close may need
/// the anchor reserve to fee-bump, so the transit wallet must not be
/// drained. A closed channel without a close record counts as force-closed.
pub(crate) fn closing_unresolved(i: &BalanceInputs) -> bool {
    let monitor_unresolved = i.monitors.iter().any(|m| {
        !m.channel_open
            && match m.kind {
                BalanceKind::CoopConfirmed => false,
                BalanceKind::OnClose => m.close != Some(CloseKind::Coop),
                BalanceKind::Resolving => true,
            }
    });
    let sweep_unconfirmed = i.sweeps.iter().any(|s| s.state != SweepState::Confirmed);
    monitor_unresolved || sweep_unconfirmed
}

/// The [`Balances`] amounts (`inbound_sat` aside), counting every sat once:
///
/// - `lightning_sat`: the total less the balances of channels no longer
///   listed, i.e. what the open channels hold.
/// - `onchain_transit_sat`: the wallet's spendable amount.
/// - `reserved_sat`: `min(reserve, total − spendable)`. Exact whenever
///   anything is spendable; when nothing is, ldk-node does not say how much
///   of the rest is confirmed, and the reserve is filled first.
/// - `pending_transit_sat`: the rest of the wallet (unconfirmed incoming),
///   less any of it that is a cooperative close still in the mempool, which
///   `pending_close_sat` already counts (at the monitor's amount, which
///   differs from the closing output by the close fee).
/// - `pending_close_sat`: closed channels' monitor balances except confirmed
///   cooperative closes (in the wallet), plus sweeper outputs whose sweep
///   the wallet does not count yet.
pub(crate) fn breakdown(i: &BalanceInputs) -> Balances {
    let closed: Vec<&MonitorBalance> = i.monitors.iter().filter(|m| !m.channel_open).collect();
    let closed_sat: u64 = closed.iter().map(|m| m.claimable_sat).sum();
    let closed_pending: u64 = closed
        .iter()
        .filter(|m| m.kind != BalanceKind::CoopConfirmed)
        .map(|m| m.claimable_sat)
        .sum();
    let coop_in_mempool: u64 = closed
        .iter()
        .filter(|m| m.kind == BalanceKind::OnClose && m.close == Some(CloseKind::Coop))
        .map(|m| m.claimable_sat)
        .sum();
    let sweeps_pending: u64 = i
        .sweeps
        .iter()
        .filter(|s| {
            matches!(
                s.state,
                SweepState::PendingBroadcast | SweepState::Broadcast { in_wallet: false }
            )
        })
        .map(|s| s.amount_sat)
        .sum();
    let w = i.wallet;
    let not_spendable = w.total_sat.saturating_sub(w.spendable_sat);
    let reserved = w.reserve_sat.min(not_spendable);
    let unconfirmed = not_spendable - reserved;
    Balances {
        lightning_sat: i.total_lightning_sat.saturating_sub(closed_sat),
        onchain_transit_sat: w.spendable_sat,
        pending_close_sat: closed_pending + sweeps_pending,
        reserved_sat: reserved,
        pending_transit_sat: unconfirmed - unconfirmed.min(coop_in_mempool),
        inbound_sat: 0,
    }
}

// ---------------------------------------------------------------------------
// Close records
// ---------------------------------------------------------------------------

fn read_closes(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read(dir.join(CLOSES_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn load_closes(dir: &Path) -> BTreeMap<String, CloseKind> {
    let _g = CLOSES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_closes(dir)
        .into_iter()
        .filter_map(|(k, v)| CloseKind::parse(&v).map(|c| (k, c)))
        .collect()
}

/// Applies `f` to the records and writes them durably if they changed.
fn update_closes(dir: &Path, f: impl FnOnce(&mut BTreeMap<String, String>)) -> std::io::Result<()> {
    let _g = CLOSES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut map = read_closes(dir);
    let before = map.clone();
    f(&mut map);
    if map == before {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&map).map_err(std::io::Error::other)?;
    crate::pay::write_atomic(dir, CLOSES_FILE, &bytes)
}

fn record_close(dir: &Path, id: &ChannelId, kind: CloseKind) -> std::io::Result<()> {
    update_closes(dir, |m| {
        m.insert(id.to_string(), kind.as_str().to_string());
    })
}

/// Called by `next_event` for every `ChannelClosed`: the reason LDK gives
/// replaces whatever the node recorded when it asked for the close.
pub(crate) fn record_closed_event(dir: &Path, id: &ChannelId, reason: Option<&ClosureReason>) {
    let _ = record_close(dir, id, CloseKind::of_reason(reason));
}

// ---------------------------------------------------------------------------
// ldk-node → model
// ---------------------------------------------------------------------------

fn not_started() -> LnError {
    LnError::Other("not_started".into())
}

/// An outbound channel whose funding transaction is not built yet
/// (`funding_txo == None`) or not yet confirmed. ldk-node builds the funding
/// transaction after `open_channel` returns and does not enter it in the
/// transit wallet until it sees it in the mempool, so until it confirms the
/// wallet may still offer its inputs to another transaction.
fn funding_in_flight(c: &ChannelDetails) -> bool {
    c.is_outbound && (c.funding_txo.is_none() || c.confirmations.unwrap_or(0) == 0)
}

struct Snapshot {
    channels: Vec<ChannelDetails>,
    closes: BTreeMap<String, CloseKind>,
    inputs: BalanceInputs,
}

impl Snapshot {
    fn take(node: &ldk_node::Node, dir: &Path) -> Snapshot {
        let channels = node.list_channels();
        let b = node.list_balances();
        let closes = load_closes(dir);
        let open: Vec<ChannelId> = channels.iter().map(|c| c.channel_id).collect();
        let monitors = b
            .lightning_balances
            .iter()
            .map(|lb| monitor_balance(lb, &open, &closes))
            .collect();
        let sweeps = b
            .pending_balances_from_channel_closures
            .iter()
            .map(|s| match s {
                PendingSweepBalance::PendingBroadcast {
                    amount_satoshis, ..
                } => SweepBalance {
                    amount_sat: *amount_satoshis,
                    state: SweepState::PendingBroadcast,
                },
                PendingSweepBalance::BroadcastAwaitingConfirmation {
                    amount_satoshis,
                    latest_spending_txid,
                    ..
                } => SweepBalance {
                    amount_sat: *amount_satoshis,
                    // ldk-node enters a wallet transaction in its payment
                    // store when its wallet sync picks it up (Esplora: the
                    // same sync that changes the balance).
                    state: SweepState::Broadcast {
                        in_wallet: node
                            .payment(&PaymentId(latest_spending_txid.to_byte_array()))
                            .is_some(),
                    },
                },
                PendingSweepBalance::AwaitingThresholdConfirmations {
                    amount_satoshis, ..
                } => SweepBalance {
                    amount_sat: *amount_satoshis,
                    state: SweepState::Confirmed,
                },
            })
            .collect();
        Snapshot {
            channels,
            closes,
            inputs: BalanceInputs {
                total_lightning_sat: b.total_lightning_balance_sats,
                monitors,
                sweeps,
                wallet: WalletBalance {
                    total_sat: b.total_onchain_balance_sats,
                    spendable_sat: b.spendable_onchain_balance_sats,
                    reserve_sat: b.total_anchor_channels_reserve_sats,
                },
            },
        }
    }

    fn state(&self) -> ChannelState {
        let pending = breakdown(&self.inputs).pending_close_sat;
        derive_state(&ChannelFacts {
            channels: self
                .channels
                .iter()
                .map(|c| ChannelFact {
                    ready: c.is_channel_ready,
                    close_recorded: self.closes.contains_key(&c.channel_id.to_string()),
                })
                .collect(),
            closed_funds_pending: pending > 0 || closing_unresolved(&self.inputs),
        })
    }

    /// Drops close records of channels that are neither listed nor hold any
    /// monitor or sweeper balance.
    fn prune(&self, node: &ldk_node::Node, dir: &Path) {
        if self.closes.is_empty() {
            return;
        }
        let b = node.list_balances();
        let mut live: Vec<String> = self
            .channels
            .iter()
            .map(|c| c.channel_id.to_string())
            .collect();
        live.extend(
            b.lightning_balances
                .iter()
                .map(|lb| balance_channel(lb).to_string()),
        );
        live.extend(
            b.pending_balances_from_channel_closures
                .iter()
                .filter_map(sweep_channel)
                .map(|c| c.to_string()),
        );
        if self.closes.keys().all(|k| live.contains(k)) {
            return;
        }
        let _ = update_closes(dir, |m| m.retain(|k, _| live.contains(k)));
    }
}

fn balance_channel(lb: &LightningBalance) -> &ChannelId {
    match lb {
        LightningBalance::ClaimableOnChannelClose { channel_id, .. }
        | LightningBalance::ClaimableAwaitingConfirmations { channel_id, .. }
        | LightningBalance::ContentiousClaimable { channel_id, .. }
        | LightningBalance::MaybeTimeoutClaimableHTLC { channel_id, .. }
        | LightningBalance::MaybePreimageClaimableHTLC { channel_id, .. }
        | LightningBalance::CounterpartyRevokedOutputClaimable { channel_id, .. } => channel_id,
    }
}

fn sweep_channel(s: &PendingSweepBalance) -> Option<ChannelId> {
    match s {
        PendingSweepBalance::PendingBroadcast { channel_id, .. }
        | PendingSweepBalance::BroadcastAwaitingConfirmation { channel_id, .. }
        | PendingSweepBalance::AwaitingThresholdConfirmations { channel_id, .. } => *channel_id,
    }
}

fn monitor_balance(
    lb: &LightningBalance,
    open: &[ChannelId],
    closes: &BTreeMap<String, CloseKind>,
) -> MonitorBalance {
    // Same amounts as LDK's `Balance::claimable_amount_satoshis`, which is
    // what `total_lightning_balance_sats` sums.
    let (claimable_sat, kind) = match lb {
        LightningBalance::ClaimableOnChannelClose {
            amount_satoshis, ..
        } => (*amount_satoshis, BalanceKind::OnClose),
        LightningBalance::ClaimableAwaitingConfirmations {
            amount_satoshis,
            source,
            ..
        } => (
            *amount_satoshis,
            if *source == BalanceSource::CoopClose {
                BalanceKind::CoopConfirmed
            } else {
                BalanceKind::Resolving
            },
        ),
        LightningBalance::ContentiousClaimable {
            amount_satoshis, ..
        }
        | LightningBalance::CounterpartyRevokedOutputClaimable {
            amount_satoshis, ..
        } => (*amount_satoshis, BalanceKind::Resolving),
        LightningBalance::MaybeTimeoutClaimableHTLC {
            amount_satoshis,
            outbound_payment,
            ..
        } => (
            if *outbound_payment {
                0
            } else {
                *amount_satoshis
            },
            BalanceKind::Resolving,
        ),
        LightningBalance::MaybePreimageClaimableHTLC { .. } => (0, BalanceKind::Resolving),
    };
    let id = balance_channel(lb);
    MonitorBalance {
        channel_open: open.contains(id),
        claimable_sat,
        kind,
        close: closes.get(&id.to_string()).copied(),
    }
}

/// [`breakdown`] of the running node, with `inbound_sat` filled in.
pub(crate) fn balances_of(node: &ldk_node::Node, dir: &Path) -> Balances {
    let snap = Snapshot::take(node, dir);
    let inbound_msat: u64 = snap
        .channels
        .iter()
        .filter(|c| c.is_channel_ready)
        .map(|c| c.inbound_capacity_msat)
        .sum();
    Balances {
        inbound_sat: inbound_msat / 1000,
        ..breakdown(&snap.inputs)
    }
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
    /// `open_channel`; ldk-node funds and broadcasts it in the background):
    /// [`channel_state`](Self::channel_state) is `Opening` until the LSP's
    /// confirmations and `channel_ready`, then `Ready` (with
    /// `LnEvent::ChannelReady`). If the LSP rejects the open, it ends with
    /// `LnEvent::ChannelClosed`.
    ///
    /// Errors, in order: `AmountOutOfRange` (msat) unless
    /// `channel_min_sat <= amount_sat <= channel_max_sat`;
    /// `Other("not_started")`; `Other("channel_opening")` while an earlier
    /// open's funding is not yet confirmed (its inputs may still look
    /// unspent to the wallet); `Other("insufficient_transit")` unless the
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
        let _ops = self.channel_ops();
        let node = &running.node;
        if node.list_channels().iter().any(funding_in_flight) {
            return Err(LnError::Other("channel_opening".into()));
        }
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

    /// Asks LDK to cooperatively close every listed channel with the LSP;
    /// the funds come back to the transit wallet once the closing
    /// transaction confirms (`channel_state` is `Closing` until then). Every
    /// listed channel goes to LDK; `Ok` only when LDK accepted the close of
    /// each. Never force-closes: see
    /// [`force_close_channel`](Self::force_close_channel).
    ///
    /// Errors: `Other("not_started")`; `Other("no_channel")`;
    /// `LspUnreachable` when the LSP cannot be reached (a cooperative close
    /// needs it online); `BackupUnavailable` from the gate or when the close
    /// record cannot be written; `Other("close_in_progress")` when LDK
    /// refuses a channel this node had already asked to close (ldk-node
    /// 0.7.1 does not pass LDK's reason on; LDK refuses a second shutdown);
    /// `Other("close_failed")` when LDK refuses otherwise (e.g. HTLCs in
    /// flight).
    pub fn close_channel(&self) -> Result<(), LnError> {
        let running = self.running().ok_or_else(not_started)?;
        let _ops = self.channel_ops();
        let node = &running.node;
        let (lsp_id, lsp_addr) = self.lsp_peer();
        let channels = node.list_channels();
        if channels.is_empty() {
            return Err(LnError::Other("no_channel".into()));
        }
        if !peer_connected(node, &lsp_id) && node.connect(lsp_id, lsp_addr, true).is_err() {
            return Err(LnError::LspUnreachable);
        }
        self.pre_send_checks()?;
        let dir = self.data_dir();
        let closes = load_closes(dir);
        let mut result = Ok(());
        for c in &channels {
            let had_record = closes.contains_key(&c.channel_id.to_string());
            if !had_record && record_close(dir, &c.channel_id, CloseKind::Coop).is_err() {
                result = result.and(Err(LnError::BackupUnavailable));
                continue;
            }
            if let Err(e) = node.close_channel(&c.user_channel_id, c.counterparty_node_id) {
                if !had_record {
                    let id = c.channel_id.to_string();
                    let _ = update_closes(dir, |m| {
                        m.remove(&id);
                    });
                }
                let err = if !peer_connected(node, &c.counterparty_node_id) {
                    LnError::LspUnreachable
                } else if e == NodeError::PersistenceFailed {
                    LnError::BackupUnavailable
                } else if had_record {
                    LnError::Other("close_in_progress".into())
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
        let _ops = self.channel_ops();
        let node = &running.node;
        let channels = node.list_channels();
        if channels.is_empty() {
            return Err(LnError::Other("no_channel".into()));
        }
        self.pre_send_checks()?;
        let mut result = Ok(());
        for c in &channels {
            let _ = record_close(self.data_dir(), &c.channel_id, CloseKind::Force);
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

    /// Sends the transit wallet's funds to `to_address` (the BTC wallet's
    /// next receive address), keeping the anchor reserve back while a
    /// channel exists. `Ok(Some(txid))` once broadcast; `Ok(None)` when the
    /// spendable transit balance is at most the dust limit (546 sat).
    ///
    /// What is sent: with no channel, ldk-node drains **every** wallet UTXO
    /// (BDK `drain_wallet`), unconfirmed incoming ones included, so the
    /// amount can exceed `onchain_transit_sat` by `pending_transit_sat`
    /// (and the transaction then confirms only after its parents). With a
    /// channel, it sends the spendable amount less the fee, from coins BDK
    /// selects, and keeps the reserve as change.
    ///
    /// Errors, in order: `Other("bad_address")` (not an address);
    /// `WrongNetwork` (not for the node's network); `Other("not_started")`;
    /// `Other("channel_opening")` while an open's funding is not yet
    /// confirmed; `Other("closing")` while a force-closed channel still
    /// resolves on chain or a sweep of its outputs is unconfirmed (the
    /// reserve may be needed to fee-bump; a cooperative close does not
    /// block); `BackupUnavailable` from the gate (nothing sent);
    /// `Other("sweep: …")` when ldk-node cannot build it (e.g. the balance
    /// does not cover the fee).
    pub fn sweep_transit(&self, to_address: &str) -> Result<Option<String>, LnError> {
        let address = Address::<NetworkUnchecked>::from_str(to_address)
            .map_err(|_| LnError::Other("bad_address".into()))?
            .require_network(self.network())
            .map_err(|_| LnError::WrongNetwork)?;
        let running = self.running().ok_or_else(not_started)?;
        let _ops = self.channel_ops();
        let node = &running.node;
        let snap = Snapshot::take(node, self.data_dir());
        if snap.channels.iter().any(funding_in_flight) {
            return Err(LnError::Other("channel_opening".into()));
        }
        if closing_unresolved(&snap.inputs) {
            return Err(LnError::Other("closing".into()));
        }
        if snap.inputs.wallet.spendable_sat <= DUST_LIMIT_SAT {
            return Ok(None);
        }
        // ldk-node's own reserve: per listed anchor channel.
        let retain_reserves = !snap.channels.is_empty();
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
    /// from ldk-node's channel list, its monitor and sweeper balances and the
    /// node's close records (module docs), all persisted, so it holds
    /// across restarts.
    pub fn channel_state(&self) -> ChannelState {
        let Some(running) = self.running() else {
            return ChannelState::None;
        };
        let snap = Snapshot::take(&running.node, self.data_dir());
        snap.prune(&running.node, self.data_dir());
        snap.state()
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

    fn ch(ready: bool, close_recorded: bool) -> ChannelFact {
        ChannelFact {
            ready,
            close_recorded,
        }
    }

    fn state(channels: &[ChannelFact], closed_funds_pending: bool) -> ChannelState {
        derive_state(&ChannelFacts {
            channels: channels.to_vec(),
            closed_funds_pending,
        })
    }

    #[test]
    fn channel_state_derivation() {
        use ChannelState as S;
        assert_eq!(state(&[], false), S::None);
        // Not ready and no close record: opening, whether the funding is
        // short of its confirmations or the LSP's channel_ready is missing.
        assert_eq!(state(&[ch(false, false)], false), S::Opening);
        // Not ready with a close record: shutting down.
        assert_eq!(state(&[ch(false, true)], false), S::Closing);
        // Ready, even with a stale record (LDK clears readiness on shutdown).
        assert_eq!(state(&[ch(true, false)], false), S::Ready);
        assert_eq!(state(&[ch(true, true)], false), S::Ready);
        // Closed, funds still resolving.
        assert_eq!(state(&[], true), S::Closing);
        // Ready wins over everything; Opening over Closing.
        assert_eq!(state(&[ch(false, true), ch(true, false)], true), S::Ready);
        assert_eq!(state(&[ch(false, false)], true), S::Opening);
        assert_eq!(
            state(&[ch(false, true), ch(false, false)], false),
            S::Opening
        );
    }

    fn mb(
        channel_open: bool,
        sat: u64,
        kind: BalanceKind,
        close: Option<CloseKind>,
    ) -> MonitorBalance {
        MonitorBalance {
            channel_open,
            claimable_sat: sat,
            kind,
            close,
        }
    }

    fn sweep(amount_sat: u64, state: SweepState) -> SweepBalance {
        SweepBalance { amount_sat, state }
    }

    fn wallet(total_sat: u64, spendable_sat: u64, reserve_sat: u64) -> WalletBalance {
        WalletBalance {
            total_sat,
            spendable_sat,
            reserve_sat,
        }
    }

    fn inputs(
        total_lightning_sat: u64,
        monitors: Vec<MonitorBalance>,
        sweeps: Vec<SweepBalance>,
        wallet: WalletBalance,
    ) -> BalanceInputs {
        BalanceInputs {
            total_lightning_sat,
            monitors,
            sweeps,
            wallet,
        }
    }

    fn sum(b: &Balances) -> u64 {
        b.lightning_sat
            + b.onchain_transit_sat
            + b.pending_close_sat
            + b.reserved_sat
            + b.pending_transit_sat
    }

    use BalanceKind::*;
    use CloseKind::*;

    #[test]
    fn breakdown_open_channel_with_reserve() {
        // Ready channel; wallet 149 844 of which 25 000 is the reserve.
        let i = inputs(
            149_056,
            vec![mb(true, 149_056, OnClose, None)],
            vec![],
            wallet(149_844, 124_844, 25_000),
        );
        let b = breakdown(&i);
        assert_eq!(
            (
                b.lightning_sat,
                b.onchain_transit_sat,
                b.reserved_sat,
                b.pending_transit_sat
            ),
            (149_056, 124_844, 25_000, 0)
        );
        assert_eq!(sum(&b), 149_056 + 149_844);
    }

    #[test]
    fn breakdown_unconfirmed_deposit_is_pending_transit() {
        let b = breakdown(&inputs(0, vec![], vec![], wallet(300_000, 0, 0)));
        assert_eq!((b.onchain_transit_sat, b.pending_transit_sat), (0, 300_000));
        // Below the reserve with a channel: the reserve is filled first.
        let b = breakdown(&inputs(0, vec![], vec![], wallet(10_000, 0, 10_000)));
        assert_eq!((b.reserved_sat, b.pending_transit_sat), (10_000, 0));
    }

    #[test]
    fn breakdown_dedups_coop_close_in_mempool() {
        // Coop close broadcast; the wallet already sees its 148 830 output
        // as unconfirmed incoming. The monitor still reports 149 056.
        let i = inputs(
            149_056,
            vec![mb(false, 149_056, OnClose, Some(Coop))],
            vec![],
            wallet(149_844 + 148_830, 149_844, 0),
        );
        let b = breakdown(&i);
        assert_eq!(b.lightning_sat, 0);
        assert_eq!(b.pending_close_sat, 149_056);
        assert_eq!(b.pending_transit_sat, 0, "not counted twice");
        assert_eq!(sum(&b), 149_844 + 149_056);
        // Not yet seen by the wallet: only pending_close counts it.
        let i = inputs(
            149_056,
            vec![mb(false, 149_056, OnClose, Some(Coop))],
            vec![],
            wallet(149_844, 149_844, 0),
        );
        assert_eq!(sum(&breakdown(&i)), 149_844 + 149_056);
        // A force close in the mempool does not pay the wallet: an unrelated
        // unconfirmed deposit is kept.
        let i = inputs(
            149_056,
            vec![mb(false, 149_056, OnClose, Some(Force))],
            vec![],
            wallet(160_000, 149_844, 0),
        );
        let b = breakdown(&i);
        assert_eq!(
            (b.pending_close_sat, b.pending_transit_sat),
            (149_056, 10_156)
        );
    }

    #[test]
    fn breakdown_confirmed_coop_close_is_in_the_wallet() {
        let i = inputs(
            148_830,
            vec![mb(false, 148_830, CoopConfirmed, Some(Coop))],
            vec![],
            wallet(298_674, 298_674, 0),
        );
        let b = breakdown(&i);
        assert_eq!((b.lightning_sat, b.pending_close_sat), (0, 0));
        assert_eq!(sum(&b), 298_674);
    }

    #[test]
    fn breakdown_force_close_and_sweeps() {
        // Timelocked to_self output.
        let i = inputs(
            149_056,
            vec![mb(false, 149_056, Resolving, Some(Force))],
            vec![],
            wallet(149_844, 149_844, 0),
        );
        assert_eq!(breakdown(&i).pending_close_sat, 149_056);
        // Handed to the sweeper: counted once in each state.
        for (state, pending) in [
            (SweepState::PendingBroadcast, 149_056),
            (SweepState::Broadcast { in_wallet: false }, 149_056),
            (SweepState::Broadcast { in_wallet: true }, 0),
            (SweepState::Confirmed, 0),
        ] {
            let i = inputs(0, vec![], vec![sweep(149_056, state)], wallet(0, 0, 0));
            assert_eq!(breakdown(&i).pending_close_sat, pending, "{state:?}");
        }
        // Mixed: one open channel, one closed channel resolving.
        let i = inputs(
            200_000,
            vec![
                mb(true, 100_000, OnClose, None),
                mb(false, 100_000, Resolving, Some(Force)),
            ],
            vec![sweep(7_000, SweepState::PendingBroadcast)],
            wallet(0, 0, 0),
        );
        let b = breakdown(&i);
        assert_eq!((b.lightning_sat, b.pending_close_sat), (100_000, 107_000));
        // Never underflows.
        let i = inputs(
            0,
            vec![mb(false, 10, Resolving, None)],
            vec![],
            wallet(5, 10, 0),
        );
        assert_eq!(breakdown(&i).lightning_sat, 0);
    }

    #[test]
    fn closing_unresolved_blocks_only_non_coop_closes() {
        let w = wallet(0, 0, 0);
        assert!(!closing_unresolved(&inputs(0, vec![], vec![], w)));
        // Open channels never block.
        assert!(!closing_unresolved(&inputs(
            1,
            vec![mb(true, 1, OnClose, None)],
            vec![],
            w
        )));
        // Coop close in the mempool or awaiting confirmations: no bump need.
        assert!(!closing_unresolved(&inputs(
            1,
            vec![mb(false, 1, OnClose, Some(Coop))],
            vec![],
            w
        )));
        assert!(!closing_unresolved(&inputs(
            1,
            vec![mb(false, 1, CoopConfirmed, Some(Coop))],
            vec![],
            w
        )));
        // Force close, unknown close kind, HTLC claims (even worth 0 to us).
        assert!(closing_unresolved(&inputs(
            1,
            vec![mb(false, 1, OnClose, Some(Force))],
            vec![],
            w
        )));
        assert!(closing_unresolved(&inputs(
            1,
            vec![mb(false, 1, OnClose, None)],
            vec![],
            w
        )));
        assert!(closing_unresolved(&inputs(
            0,
            vec![mb(false, 0, Resolving, Some(Coop))],
            vec![],
            w
        )));
        // Sweeps until confirmed.
        for (state, blocks) in [
            (SweepState::PendingBroadcast, true),
            (SweepState::Broadcast { in_wallet: true }, true),
            (SweepState::Confirmed, false),
        ] {
            assert_eq!(
                closing_unresolved(&inputs(0, vec![], vec![sweep(1, state)], w)),
                blocks,
                "{state:?}"
            );
        }
    }

    #[test]
    fn close_kind_from_reason() {
        assert_eq!(
            CloseKind::of_reason(Some(&ClosureReason::LocallyInitiatedCooperativeClosure)),
            Coop
        );
        assert_eq!(
            CloseKind::of_reason(Some(
                &ClosureReason::CounterpartyInitiatedCooperativeClosure
            )),
            Coop
        );
        assert_eq!(
            CloseKind::of_reason(Some(&ClosureReason::CommitmentTxConfirmed)),
            Force
        );
        assert_eq!(CloseKind::of_reason(None), Force);
    }

    #[test]
    fn close_records_round_trip() {
        let dir = std::env::temp_dir().join(format!("fortis-ln-closes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = ChannelId([1; 32]);
        let b = ChannelId([2; 32]);
        record_close(&dir, &a, Coop).unwrap();
        record_close(&dir, &b, Force).unwrap();
        // The event's reason replaces what the node recorded.
        record_closed_event(&dir, &a, Some(&ClosureReason::CommitmentTxConfirmed));
        let m = load_closes(&dir);
        assert_eq!(m.get(&a.to_string()), Some(&Force));
        assert_eq!(m.get(&b.to_string()), Some(&Force));
        update_closes(&dir, |m| m.retain(|k, _| *k == b.to_string())).unwrap();
        assert_eq!(load_closes(&dir).len(), 1);
        // Unreadable file: no records (never evidence of a shutdown).
        std::fs::write(dir.join(CLOSES_FILE), b"not json").unwrap();
        assert!(load_closes(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
