//! [`Node::pay`], [`Node::payments`] and [`Node::estimate_fee_msat`].
//!
//! **Pay.** `pay` takes a [`PaymentTarget`] and checks it against the
//! node's *own* network and the clock, whatever `parse` was told:
//!
//! 1. target checks (no I/O except LNURL): on-chain targets are
//!    `Other("not_lightning")`; a unified target pays its Lightning part;
//!    LNURL-pay and Lightning addresses are resolved (Task 3) to a BOLT11
//!    invoice whose description hash must match the LNURL metadata. Then
//!    `WrongNetwork`, `InvoiceExpired`, `Other("pay_self")` (payee is this
//!    node) and the amount rules: an invoice with an amount is paid for
//!    exactly that (a different `amount_msat` is `Other("amount_mismatch")`),
//!    one without needs `amount_msat` (`Other("amount_required")`);
//! 2. `Other("not_started")` unless the node runs;
//! 3. [`Node::pre_send_checks`]: the device-lock check, one VSS round trip
//!    (≤ 3 s) — `BackupUnavailable` and nothing is sent when VSS does not
//!    answer ("Lightning refuses to pay while VSS is unreachable");
//!    `ActiveOnAnotherDevice` when another device took over (this node then
//!    stops itself, `LnEvent::DeviceLockLost`);
//! 4. `InsufficientLightning` when the amount exceeds what the usable
//!    channels can send now (LDK's `next_outbound_htlc_limit_msat`, which
//!    already keeps the channel reserve);
//! 5. the HTLC goes to ldk-node (`bolt11_payment().send` /
//!    `send_using_amount`, `bolt12_payment().send` / `send_using_amount`).
//!    A synchronous routing failure is `NoRoute`; a failed persist of the
//!    payment record *after* the HTLC went out is `PaymentStateUnknown`
//!    (see [`Node::pay`]), guarded by a local attempt record so the same
//!    invoice is never sent twice while its outcome is unknown.
//!
//! **Exit mode.** On a node started with
//! [`StartMode::Exit`](crate::StartMode::Exit), `pay` returns
//! `LspUnreachable` before step 1, at once, while the LSP peer is not
//! connected (all routes go through it).
//!
//! `pay` returns once the payment is handed to LDK; the outcome arrives as
//! `LnEvent::PaymentSent` / `PaymentFailed` with the same id, and in
//! [`Node::payments`]. The id is the hex of ldk-node's payment id (the
//! payment hash for BOLT11).
//!
//! **Descriptions.** ldk-node's payment store keeps no description, so the
//! node notes each one (sent invoice, LNURL text, receive description) in
//! `<data_dir>/fortis-payment-notes.json`. That file is local only, not in
//! VSS: after a restore on a new device the history has amounts and states
//! but empty descriptions.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ldk_node::bitcoin::constants::ChainHash;
use ldk_node::bitcoin::hashes::Hash;
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::offers::offer::{Amount, Offer};
use ldk_node::lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};
use ldk_node::payment::{PaymentDetails, PaymentDirection, PaymentKind, PaymentStatus};
use ldk_node::NodeError;

use crate::lnurl::{self, HttpGet, UreqGet};
use crate::{LnError, Node, PaymentTarget};

/// Cap on the device-lock check (VSS round trip) before a payment.
const PAY_VSS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const NOTES_FILE: &str = "fortis-payment-notes.json";
/// BOLT11 sends handed to LDK whose outcome is not yet in ldk-node's store:
/// payment id (hex) → unix seconds after which the record is ignored.
const ATTEMPTS_FILE: &str = "fortis-payment-attempts.json";
/// Kept this long past the invoice's own expiry.
const ATTEMPT_MARGIN_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Succeeded,
    Failed,
}

/// One entry of the payment history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    /// Hex payment id; the id `pay` returned and that events carry.
    pub id: String,
    pub direction: Direction,
    /// Out: what the recipient gets (excluding `fee_msat`). In: what this
    /// node received (for a JIT receive, net of the LSP's opening fee).
    pub amount_msat: u64,
    /// Out: the routing fee paid (0 until it succeeded). In: the LSP's
    /// channel-opening fee for a JIT receive, else 0.
    pub fee_msat: u64,
    pub status: Status,
    pub description: String,
    /// Unix seconds of the last status change.
    pub timestamp: u64,
}

/// What `pay` hands to ldk-node.
enum Prepared {
    Bolt11 {
        invoice: Box<Bolt11Invoice>,
        /// `Some` for a no-amount invoice (`send_using_amount`).
        amount_override: Option<u64>,
        amount_msat: u64,
        description: String,
    },
    Bolt12 {
        offer: Box<Offer>,
        amount_override: Option<u64>,
        amount_msat: u64,
        description: String,
    },
}

impl Prepared {
    fn amount_msat(&self) -> u64 {
        match self {
            Prepared::Bolt11 { amount_msat, .. } | Prepared::Bolt12 { amount_msat, .. } => {
                *amount_msat
            }
        }
    }
}

impl Node {
    /// Pays `target`; `amount_msat` is required for a target without an
    /// amount (no-amount invoice or offer, LNURL) and must equal the amount
    /// of one that has it. See the module docs for the order of checks.
    ///
    /// **Blocking.** Up to 3 s for the VSS check, plus the LNURL round trips
    /// (≤ 10 s each) for LNURL targets: call it off the UI thread.
    ///
    /// **`BackupUnavailable` vs `PaymentStateUnknown`.** `BackupUnavailable`
    /// comes only from the pre-send gate: nothing was sent.
    /// `PaymentStateUnknown` means ldk-node failed to persist the payment
    /// *after* the HTLC (or BOLT12 invoice request) went out: money may be
    /// moving. The node keeps a local "attempted" record of the invoice
    /// (written before the send), and while ldk-node's store has no entry
    /// for it and it has not expired, paying the same invoice again also
    /// returns `PaymentStateUnknown` instead of sending a second HTLC.
    pub fn pay(&self, target: &PaymentTarget, amount_msat: Option<u64>) -> Result<String, LnError> {
        self.pay_with_http(target, amount_msat, &UreqGet::new())
    }

    /// History, both directions, most recent first: sends (any state) and
    /// completed receives. Unpaid invoices from `receive` and on-chain
    /// entries of ldk-node's transit wallet are not listed. Empty when the
    /// node is not running.
    pub fn payments(&self) -> Vec<Payment> {
        let Some(running) = self.running() else {
            return Vec::new();
        };
        let notes = Notes::load(self.data_dir());
        let mut list: Vec<Payment> = running
            .node
            .list_payments()
            .into_iter()
            .filter_map(|p| to_payment(&p, &notes))
            .collect();
        list.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then(a.id.cmp(&b.id)));
        list
    }

    /// The most ldk-node will spend on routing fees for this payment: its
    /// default cap of 1 % of the amount + 50 sat (LDK
    /// `RouteParameters::from_payment_params_and_value`; fortis sets no
    /// `route_parameters`). An upper bound, not the expected fee — LDK has
    /// no pre-send estimate. The amount is the target's own when it has one,
    /// else `amount_msat`.
    pub fn estimate_fee_msat(&self, target: &PaymentTarget, amount_msat: u64) -> u64 {
        estimate_routing_fee_msat(target, amount_msat)
    }

    /// What [`pay`](Self::pay) can send now: LDK's `next_outbound_htlc_limit_msat`
    /// summed over the usable channels (the channel reserve is already kept
    /// back). Zero when the node is not running.
    pub fn sendable_msat(&self) -> u64 {
        self.running().map_or(0, |r| sendable_msat(&r.node))
    }

    /// [`pay`](Self::pay) with the LNURL HTTP client injected.
    fn pay_with_http(
        &self,
        target: &PaymentTarget,
        amount_msat: Option<u64>,
        http: &dyn HttpGet,
    ) -> Result<String, LnError> {
        // Exit mode, LSP not connected: refused before anything (LNURL
        // lookups included). A node not running gets the usual order.
        if let Some(running) = self.running() {
            self.exit_gate(&running)?;
        }
        let prepared = self.prepare(target, amount_msat, http)?;
        let running = self
            .running()
            .ok_or_else(|| LnError::Other("not_started".into()))?;
        let node: &ldk_node::Node = &running.node;
        let amount = prepared.amount_msat();

        match prepared {
            Prepared::Bolt11 {
                invoice,
                amount_override,
                description,
                ..
            } => {
                let pid = PaymentId(invoice.payment_hash().to_byte_array());
                let id_hex = hex::encode(pid.0);
                let expires_at = invoice
                    .expires_at()
                    .map_or(u64::MAX, |d| d.as_secs())
                    .saturating_add(ATTEMPT_MARGIN_SECS);
                let p = node.bolt11_payment();
                let res = guarded_bolt11(
                    self.data_dir(),
                    &id_hex,
                    expires_at,
                    now().as_secs(),
                    || node.payment(&pid).is_some(),
                    || self.liquidity_and_gate(node, amount),
                    || match amount_override {
                        Some(a) => p.send_using_amount(&invoice, a, None),
                        None => p.send(&invoice, None),
                    },
                );
                if matches!(res, Ok(_) | Err(LnError::PaymentStateUnknown)) {
                    self.note_description(&id_hex, &description);
                }
                res
            }
            Prepared::Bolt12 {
                offer,
                amount_override,
                description,
                ..
            } => {
                self.liquidity_and_gate(node, amount)?;
                // Same ordering as BOLT11 (invoice request out, then the
                // store insert), so the same error mapping. No attempt
                // record: the id is random per send and an offer may be
                // paid more than once on purpose.
                let p = node.bolt12_payment();
                let id = match amount_override {
                    Some(a) => p.send_using_amount(&offer, a, None, None, None),
                    None => p.send(&offer, None, None, None),
                }
                .map_err(map_send_error)?;
                let id_hex = hex::encode(id.0);
                self.note_description(&id_hex, &description);
                Ok(id_hex)
            }
        }
    }

    /// Steps 3 and 4: [`Self::pre_send_checks`], then
    /// `InsufficientLightning`. The gate comes first: a node whose lock was
    /// taken over may also have lost its channel's peer connection to the
    /// new device, and must report (and act on) the takeover, not the
    /// liquidity.
    fn liquidity_and_gate(&self, node: &ldk_node::Node, amount_msat: u64) -> Result<(), LnError> {
        self.pre_send_checks()?;
        if amount_msat > sendable_msat(node) {
            return Err(LnError::InsufficientLightning);
        }
        Ok(())
    }

    /// The checks every send passes right before the HTLC goes to LDK:
    /// this node still holds the device lock, by a conditional rewrite of it
    /// in VSS (≤ 3 s, one round trip, which is also the "VSS answers"
    /// check). `BackupUnavailable` when VSS does not answer;
    /// `ActiveOnAnotherDevice` when another device took over (the node is
    /// then stopped and `DeviceLockLost` queued, see
    /// [`Node::check_device_lock`]). Also gates the channel and transit
    /// calls (`channel.rs`).
    pub(crate) fn pre_send_checks(&self) -> Result<(), LnError> {
        self.check_device_lock(PAY_VSS_PROBE_TIMEOUT)
    }

    /// Target checks (step 1 of the module docs).
    fn prepare(
        &self,
        target: &PaymentTarget,
        amount_msat: Option<u64>,
        http: &dyn HttpGet,
    ) -> Result<Prepared, LnError> {
        match target {
            PaymentTarget::Bolt11 { invoice, .. } => {
                let inv = parse_invoice(invoice)?;
                self.check_bolt11(inv, amount_msat, None)
            }
            PaymentTarget::Bolt12Offer { offer, .. } => self.check_offer(offer, amount_msat),
            PaymentTarget::LnurlPay { .. } | PaymentTarget::LightningAddress { .. } => {
                let amount = amount_msat
                    .filter(|a| *a > 0)
                    .ok_or_else(|| LnError::Other("amount_required".into()))?;
                let params = lnurl::resolve(target, http)?;
                let pr = lnurl::request_invoice(&params, amount, http)?;
                let inv = parse_invoice(&pr)?;
                self.check_bolt11(inv, Some(amount), Some(params.description))
            }
            PaymentTarget::Unified { lightning, .. } => match lightning.as_ref() {
                PaymentTarget::Unified { .. } => Err(LnError::Other("not_lightning".into())),
                inner => self.prepare(inner, amount_msat, http),
            },
            PaymentTarget::OnChain { .. } => Err(LnError::Other("not_lightning".into())),
        }
    }

    fn check_bolt11(
        &self,
        inv: Bolt11Invoice,
        amount_msat: Option<u64>,
        description: Option<String>,
    ) -> Result<Prepared, LnError> {
        if inv.network() != self.network() {
            return Err(LnError::WrongNetwork);
        }
        if inv.would_expire(now()) {
            return Err(LnError::InvoiceExpired);
        }
        if inv.recover_payee_pub_key().to_string() == self.node_id() {
            return Err(LnError::Other("pay_self".into()));
        }
        let (amount_override, amount) = fixed_or_given(inv.amount_milli_satoshis(), amount_msat)?;
        let description = description.unwrap_or_else(|| match inv.description() {
            Bolt11InvoiceDescriptionRef::Direct(d) => d.to_string(),
            Bolt11InvoiceDescriptionRef::Hash(_) => String::new(),
        });
        Ok(Prepared::Bolt11 {
            invoice: Box::new(inv),
            amount_override,
            amount_msat: amount,
            description,
        })
    }

    fn check_offer(&self, offer: &str, amount_msat: Option<u64>) -> Result<Prepared, LnError> {
        let offer = Offer::from_str(offer.trim())
            .map_err(|e| LnError::Other(format!("invalid offer: {e:?}")))?;
        if !offer.supports_chain(ChainHash::using_genesis_block(self.network())) {
            return Err(LnError::WrongNetwork);
        }
        if offer.is_expired_no_std(now()) {
            return Err(LnError::InvoiceExpired);
        }
        if offer
            .issuer_signing_pubkey()
            .is_some_and(|k| k.to_string() == self.node_id())
        {
            return Err(LnError::Other("pay_self".into()));
        }
        let fixed = match offer.amount() {
            None => None,
            Some(Amount::Bitcoin { amount_msats }) => Some(amount_msats),
            Some(Amount::Currency { .. }) => {
                return Err(LnError::Other("unsupported_currency".into()))
            }
        };
        let (amount_override, amount) = fixed_or_given(fixed, amount_msat)?;
        let description = offer
            .description()
            .map(|d| d.to_string())
            .unwrap_or_default();
        Ok(Prepared::Bolt12 {
            offer: Box::new(offer),
            amount_override,
            amount_msat: amount,
            description,
        })
    }

    /// Notes `description` for payment `id` (best effort; cosmetic).
    pub(crate) fn note_description(&self, id: &str, description: &str) {
        if description.is_empty() {
            return;
        }
        Notes::update(self.data_dir(), |n| {
            n.insert(id.to_string(), description.to_string());
        });
    }
}

/// `(amount_override, amount)`: an amount in the target wins and must match
/// `given` if any; otherwise `given` is required (and nonzero).
fn fixed_or_given(fixed: Option<u64>, given: Option<u64>) -> Result<(Option<u64>, u64), LnError> {
    match (fixed, given) {
        (Some(f), None) => Ok((None, f)),
        (Some(f), Some(g)) if f == g => Ok((None, f)),
        (Some(_), Some(_)) => Err(LnError::Other("amount_mismatch".into())),
        (None, Some(g)) if g > 0 => Ok((Some(g), g)),
        (None, _) => Err(LnError::Other("amount_required".into())),
    }
}

/// The routing fee [`Node::pay`] allows for `target` (its own amount when it
/// has one, else `amount_msat`): the one rule the node, the FFI and the app's
/// preview all use. It needs no node.
pub fn estimate_routing_fee_msat(target: &PaymentTarget, amount_msat: u64) -> u64 {
    routing_fee_cap_msat(target_amount_msat(target).unwrap_or(amount_msat))
}

/// LDK's default maximum total routing fee for a payment of `amount_msat`:
/// 1 % + 50 sat.
fn routing_fee_cap_msat(amount_msat: u64) -> u64 {
    amount_msat / 100 + 50_000
}

/// The amount the target itself fixes, if any.
fn target_amount_msat(target: &PaymentTarget) -> Option<u64> {
    match target {
        PaymentTarget::Bolt11 { invoice, .. } => parse_invoice(invoice)
            .ok()
            .and_then(|i| i.amount_milli_satoshis()),
        PaymentTarget::Bolt12Offer { offer, .. } => {
            match Offer::from_str(offer.trim()).ok()?.amount() {
                Some(Amount::Bitcoin { amount_msats }) => Some(amount_msats),
                _ => None,
            }
        }
        PaymentTarget::Unified { lightning, .. } => target_amount_msat(lightning),
        _ => None,
    }
}

fn parse_invoice(s: &str) -> Result<Bolt11Invoice, LnError> {
    Bolt11Invoice::from_str(&s.trim().to_ascii_lowercase())
        .map_err(|e| LnError::Other(format!("invalid invoice: {e}")))
}

/// What `pay` can send now (step 3): the sum of LDK's
/// `next_outbound_htlc_limit_msat` over usable channels.
pub(crate) fn sendable_msat(node: &ldk_node::Node) -> u64 {
    node.list_channels()
        .iter()
        .filter(|c| c.is_usable)
        .map(|c| c.next_outbound_htlc_limit_msat)
        .sum()
}

pub(crate) fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Maps an error of ldk-node's `send*` (called only after the pre-send gate
/// passed). In ldk-node 0.7.1 the HTLC / invoice request is handed to LDK
/// *before* the payment-store insert, so `PersistenceFailed` here means
/// "possibly in flight": `PaymentStateUnknown`, never `BackupUnavailable`
/// (which is reserved for the gate, when nothing was sent).
fn map_send_error(e: NodeError) -> LnError {
    match e {
        NodeError::NotRunning => LnError::Other("not_started".into()),
        NodeError::PaymentSendingFailed => LnError::NoRoute,
        NodeError::PersistenceFailed => LnError::PaymentStateUnknown,
        NodeError::DuplicatePayment => LnError::Other("duplicate_payment".into()),
        NodeError::UnsupportedCurrency => LnError::Other("unsupported_currency".into()),
        other => LnError::Other(format!("pay: {other}")),
    }
}

/// History entry for an ldk-node payment, or `None` for what the history
/// leaves out (on-chain transit, receives not completed).
fn to_payment(p: &PaymentDetails, notes: &Notes) -> Option<Payment> {
    let (fee_msat, jit) = match &p.kind {
        PaymentKind::Onchain { .. } => return None,
        PaymentKind::Bolt11Jit {
            counterparty_skimmed_fee_msat,
            lsp_fee_limits,
            ..
        } => (
            counterparty_skimmed_fee_msat
                .or(lsp_fee_limits.max_total_opening_fee_msat)
                .unwrap_or(0),
            true,
        ),
        _ => (p.fee_paid_msat.unwrap_or(0), false),
    };
    let direction = match p.direction {
        PaymentDirection::Inbound => Direction::In,
        PaymentDirection::Outbound => Direction::Out,
    };
    let status = match p.status {
        PaymentStatus::Pending => Status::Pending,
        PaymentStatus::Succeeded => Status::Succeeded,
        PaymentStatus::Failed => Status::Failed,
    };
    if direction == Direction::In && status != Status::Succeeded {
        return None;
    }
    let id = hex::encode(p.id.0);
    Some(Payment {
        description: notes.0.get(&id).cloned().unwrap_or_default(),
        id,
        direction,
        amount_msat: p.amount_msat.unwrap_or(0),
        fee_msat: if direction == Direction::In && !jit {
            0
        } else {
            fee_msat
        },
        status,
        timestamp: p.latest_update_timestamp,
    })
}

/// Hex payment hashes with a `pay` between its attempt check and the end of
/// its send in this process.
static IN_FLIGHT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Holds `id` in [`IN_FLIGHT`] until dropped.
struct HashGuard(String);

impl HashGuard {
    /// `PaymentStateUnknown` when another `pay` of the same invoice is
    /// running: the second caller is refused, not queued (its outcome
    /// depends on the first one's, which the caller learns from events and
    /// `payments()`). Different hashes never block each other.
    fn claim(id: &str) -> Result<HashGuard, LnError> {
        let mut set = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(id.to_string()) {
            return Err(LnError::PaymentStateUnknown);
        }
        Ok(HashGuard(id.to_string()))
    }
}

impl Drop for HashGuard {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// The BOLT11 send, serialised per payment hash: claim the hash, check the
/// attempt record, run `before_send` (liquidity, pre-send gate), then
/// record → send → settle the record ([`send_recorded`]).
fn guarded_bolt11(
    dir: &Path,
    id: &str,
    expires_at: u64,
    now_secs: u64,
    in_ldk_store: impl Fn() -> bool,
    before_send: impl FnOnce() -> Result<(), LnError>,
    send: impl FnOnce() -> Result<PaymentId, NodeError>,
) -> Result<String, LnError> {
    let _guard = HashGuard::claim(id)?;
    check_attempt(dir, id, now_secs, in_ldk_store)?;
    before_send()?;
    send_recorded(dir, id, expires_at, now_secs, send)
}

fn unreadable() -> LnError {
    LnError::Other("attempt_record_unreadable".into())
}

/// `Ok` unless `id` has an unexpired attempt record and ldk-node's store
/// (`in_ldk_store`) has no entry for it: then `PaymentStateUnknown`. A
/// record that is expired or whose payment ldk-node lists is dropped. An
/// attempts file that cannot be read or parsed refuses the payment
/// (`Other("attempt_record_unreadable")`) and is left as it is.
fn check_attempt(
    dir: &Path,
    id: &str,
    now_secs: u64,
    in_ldk_store: impl Fn() -> bool,
) -> Result<(), LnError> {
    let Some(expires_at) = Attempts::load(dir).map_err(|_| unreadable())?.get(id) else {
        return Ok(());
    };
    if expires_at > now_secs && !in_ldk_store() {
        return Err(LnError::PaymentStateUnknown);
    }
    let _ = Attempts::update(dir, now_secs, |a| {
        a.remove(id);
    });
    Ok(())
}

/// Records the attempt for `id` (durably, pruning expired ones), runs
/// `send`, then settles the record:
/// - `PersistenceFailed` (outcome unknown): the record must stay; it is
///   written back if it went missing.
/// - `DuplicatePayment`: another attempt owns the hash; left alone.
/// - anything else (sent and stored, or refused before sending): cleared.
///
/// Nothing is sent if the record cannot be written (or the file read).
fn send_recorded(
    dir: &Path,
    id: &str,
    expires_at: u64,
    now_secs: u64,
    send: impl FnOnce() -> Result<PaymentId, NodeError>,
) -> Result<String, LnError> {
    let record = |a: &mut BTreeMap<String, u64>| {
        a.insert(id.to_string(), expires_at);
    };
    Attempts::update(dir, now_secs, record).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::PermissionDenied => unreadable(),
        _ => LnError::Other(format!("attempt record: {e}")),
    })?;
    let res = send();
    match &res {
        Err(NodeError::PersistenceFailed) => {
            if Attempts::load(dir).ok().and_then(|a| a.get(id)) != Some(expires_at) {
                let _ = Attempts::update(dir, now_secs, record);
            }
        }
        Err(NodeError::DuplicatePayment) => {}
        _ => {
            let _ = Attempts::update(dir, now_secs, |a| {
                a.remove(id);
            });
        }
    }
    res.map(|p| hex::encode(p.0)).map_err(map_send_error)
}

/// Serialises read-modify-write of the data-dir JSON files in this process.
static FILES_LOCK: Mutex<()> = Mutex::new(());

/// Writes `bytes` to `dir/file` atomically and durably (temp file, fsync,
/// rename, fsync of the directory).
pub(crate) fn write_atomic(dir: &Path, file: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{file}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(file))?;
    std::fs::File::open(dir)?.sync_all()
}

/// Payment id → description, kept in the data dir (see the module docs).
#[derive(Default)]
struct Notes(BTreeMap<String, String>);

impl Notes {
    fn load(dir: &Path) -> Notes {
        let _g = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Notes::read(dir)
    }

    fn read(dir: &Path) -> Notes {
        std::fs::read(dir.join(NOTES_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .map(Notes)
            .unwrap_or_default()
    }

    fn update(dir: &Path, f: impl FnOnce(&mut BTreeMap<String, String>)) {
        let _g = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut n = Notes::read(dir);
        f(&mut n.0);
        if let Ok(bytes) = serde_json::to_vec(&n.0) {
            let _ = write_atomic(dir, NOTES_FILE, &bytes);
        }
    }
}

/// Attempted BOLT11 sends (see [`ATTEMPTS_FILE`]).
#[derive(Default)]
struct Attempts(BTreeMap<String, u64>);

impl Attempts {
    fn load(dir: &Path) -> std::io::Result<Attempts> {
        let _g = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Attempts::read(dir)
    }

    /// Fails closed: only a missing file is an empty set; any other read
    /// error, or bytes that do not parse, is an error (`InvalidData` for
    /// the latter).
    fn read(dir: &Path) -> std::io::Result<Attempts> {
        match std::fs::read(dir.join(ATTEMPTS_FILE)) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Attempts)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Attempts::default()),
            Err(e) => Err(e),
        }
    }

    fn get(&self, id: &str) -> Option<u64> {
        self.0.get(id).copied()
    }

    /// Applies `f`, drops records expired at `now_secs`, writes durably.
    /// Never writes over a file it could not read.
    fn update(
        dir: &Path,
        now_secs: u64,
        f: impl FnOnce(&mut BTreeMap<String, u64>),
    ) -> std::io::Result<()> {
        let _g = FILES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut a = Attempts::read(dir)?;
        f(&mut a.0);
        a.0.retain(|_, exp| *exp > now_secs);
        let bytes = serde_json::to_vec(&a.0).map_err(std::io::Error::other)?;
        write_atomic(dir, ATTEMPTS_FILE, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lnurl::tests::signed_invoice;
    use crate::{ChainSource, LspInfo, LspLimits, NodeConfig, VssConfig};
    use ldk_node::bitcoin::Network;
    use serde_json::Value;
    use std::collections::HashMap;

    #[test]
    fn routing_fee_cap_is_one_percent_plus_50_sat() {
        assert_eq!(routing_fee_cap_msat(0), 50_000);
        assert_eq!(routing_fee_cap_msat(1_000_000), 60_000);
        assert_eq!(routing_fee_cap_msat(100_000_000), 1_050_000);
        assert_eq!(routing_fee_cap_msat(199), 50_001);
    }

    #[test]
    fn amount_rules() {
        assert_eq!(fixed_or_given(Some(5), None), Ok((None, 5)));
        assert_eq!(fixed_or_given(Some(5), Some(5)), Ok((None, 5)));
        assert_eq!(
            fixed_or_given(Some(5), Some(6)),
            Err(LnError::Other("amount_mismatch".into()))
        );
        assert_eq!(fixed_or_given(None, Some(7)), Ok((Some(7), 7)));
        for given in [None, Some(0)] {
            assert_eq!(
                fixed_or_given(None, given),
                Err(LnError::Other("amount_required".into()))
            );
        }
    }

    struct Fake(HashMap<String, Value>);
    impl HttpGet for Fake {
        fn get_json(&self, url: &str) -> Result<Value, LnError> {
            self.0
                .get(url)
                .cloned()
                .ok_or_else(|| LnError::Other(format!("404 {url}")))
        }
    }

    const META: &str = "[[\"text/plain\",\"coffee\"]]";

    fn fake(invoice: Option<String>) -> Fake {
        let mut m = HashMap::new();
        m.insert(
            "https://e.com/.well-known/lnurlp/bob".to_string(),
            serde_json::json!({
                "tag": "payRequest", "callback": "https://e.com/cb",
                "minSendable": 1_000, "maxSendable": 10_000_000, "metadata": META
            }),
        );
        if let Some(inv) = invoice {
            m.insert(
                "https://e.com/cb?amount=5000".to_string(),
                serde_json::json!({ "pr": inv }),
            );
        }
        Fake(m)
    }

    fn node(dir: &str) -> Node {
        Node::new(node_cfg(dir)).unwrap()
    }

    fn node_cfg(dir: &str) -> NodeConfig {
        NodeConfig {
            bip39_seed: vec![3; 64],
            network: Network::Regtest,
            data_dir: std::env::temp_dir().join(dir),
            chain_source: ChainSource::Esplora {
                url: "http://192.0.2.1".into(),
                auth_header: None,
            },
            lsp: LspInfo {
                node_id: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                    .into(),
                host: "192.0.2.1".into(),
                port: 9735,
                token: None,
            },
            vss: VssConfig {
                url: "http://192.0.2.1/vss".into(),
                auth_header: String::new(),
            },
            limits: LspLimits {
                min_payment_msat: 1,
                max_payment_msat: 2,
                channel_min_sat: 3,
                channel_max_sat: 4,
            },
        }
    }

    fn bob() -> PaymentTarget {
        PaymentTarget::LightningAddress {
            user: "bob".into(),
            domain: "e.com".into(),
        }
    }

    #[test]
    fn lnurl_target_goes_through_resolve_and_hash_check() {
        let n = node("fortis-pay-unit-lnurl");
        // Amount: required, and inside the LNURL range (no callback fetched).
        let f = fake(None);
        assert_eq!(
            n.pay_with_http(&bob(), None, &f),
            Err(LnError::Other("amount_required".into()))
        );
        assert_eq!(
            n.pay_with_http(&bob(), Some(999), &f),
            Err(LnError::AmountOutOfRange {
                min_msat: 1_000,
                max_msat: 10_000_000
            })
        );
        // Hash mismatch / plain description: rejected before anything else.
        for bad in [
            signed_invoice(5_000, "[[\"text/plain\",\"tea\"]]", false),
            signed_invoice(5_000, META, true),
        ] {
            assert_eq!(
                n.pay_with_http(&bob(), Some(5_000), &fake(Some(bad))),
                Err(LnError::Other("lnurl_description_hash".into()))
            );
        }
        // A good invoice passes every target check; the node is not running.
        let good = signed_invoice(5_000, META, false);
        assert_eq!(
            n.pay_with_http(&bob(), Some(5_000), &fake(Some(good))),
            Err(LnError::Other("not_started".into()))
        );
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fortis-pay-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn send_errors_after_the_gate() {
        // The HTLC may be out when ldk-node fails to persist the payment.
        assert_eq!(
            map_send_error(NodeError::PersistenceFailed),
            LnError::PaymentStateUnknown
        );
        assert_eq!(
            map_send_error(NodeError::PaymentSendingFailed),
            LnError::NoRoute
        );
        assert_eq!(
            map_send_error(NodeError::DuplicatePayment),
            LnError::Other("duplicate_payment".into())
        );
    }

    #[test]
    fn gate_failure_is_backup_unavailable() {
        // Port 1 on loopback: refused at once.
        let n = Node::new(NodeConfig {
            vss: VssConfig {
                url: "http://127.0.0.1:1/vss".into(),
                auth_header: String::new(),
            },
            ..node_cfg("fortis-pay-unit-gate")
        })
        .unwrap();
        assert_eq!(n.pre_send_checks(), Err(LnError::Other("not_started".into())));
        // Holding the lock, the check is a VSS round trip: refused.
        n.hold_lock_for_test();
        assert_eq!(
            n.check_lock_at(0, PAY_VSS_PROBE_TIMEOUT),
            Err(LnError::BackupUnavailable)
        );
        // Unreachable is not a lost lock: still held.
        assert_eq!(
            n.check_lock_at(0, PAY_VSS_PROBE_TIMEOUT),
            Err(LnError::BackupUnavailable)
        );
        assert_eq!(n.next_event(Duration::ZERO), None);
    }

    const ID: &str = "abababababababababababababababababababababababababababababababab";

    #[test]
    fn attempt_recorded_before_send_and_kept_when_state_unknown() {
        let dir = tmp_dir("attempt-unknown");
        let r = send_recorded(&dir, ID, 2_000, 1_000, || {
            // The record is on disk before ldk-node is called.
            assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
            Err(NodeError::PersistenceFailed)
        });
        assert_eq!(r, Err(LnError::PaymentStateUnknown));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
        // A second pay for the same hash, ldk-node has no entry: refused.
        assert_eq!(
            check_attempt(&dir, ID, 1_500, || false),
            Err(LnError::PaymentStateUnknown)
        );
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attempt_cleared_once_ldk_lists_it_or_it_expires() {
        let dir = tmp_dir("attempt-clear");
        let _ = send_recorded(&dir, ID, 2_000, 1_000, || Err(NodeError::PersistenceFailed));
        // ldk-node's store lists it (any status): the record goes.
        assert_eq!(check_attempt(&dir, ID, 1_500, || true), Ok(()));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), None);

        let _ = send_recorded(&dir, ID, 2_000, 1_000, || Err(NodeError::PersistenceFailed));
        // Expired: ignored and dropped.
        assert_eq!(check_attempt(&dir, ID, 2_000, || false), Ok(()));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn attempt_cleared_when_ldk_took_or_refused_it() {
        let dir = tmp_dir("attempt-done");
        let ok = send_recorded(&dir, ID, 2_000, 1_000, || Ok(PaymentId([0xab; 32])));
        assert_eq!(ok, Ok(ID.to_string()));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), None);
        let no_route = send_recorded(&dir, ID, 2_000, 1_000, || {
            Err(NodeError::PaymentSendingFailed)
        });
        assert_eq!(no_route, Err(LnError::NoRoute));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expired_attempts_pruned_on_write() {
        let dir = tmp_dir("attempt-prune");
        let other = "cd".repeat(32);
        let _ = send_recorded(&dir, &other, 1_100, 1_000, || {
            Err(NodeError::PersistenceFailed)
        });
        let _ = send_recorded(&dir, ID, 5_000, 2_000, || Err(NodeError::PersistenceFailed));
        let a = Attempts::load(&dir).unwrap();
        assert_eq!((a.get(&other), a.get(ID)), (None, Some(5_000)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn guarded(
        dir: &Path,
        id: &str,
        sends: &std::sync::atomic::AtomicU32,
        result: Result<PaymentId, NodeError>,
        hold: Duration,
    ) -> Result<String, LnError> {
        guarded_bolt11(
            dir,
            id,
            2_000,
            1_000,
            || false,
            // Held after the attempt check, so that without the per-hash
            // guard both callers would pass the check before either records.
            || {
                std::thread::sleep(hold);
                Ok(())
            },
            || {
                sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(hold);
                result
            },
        )
    }

    #[test]
    fn concurrent_pays_of_one_hash_send_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Barrier};
        // Its own hash: `IN_FLIGHT` is process-wide and tests run in parallel.
        let id = "c0".repeat(32);
        let dir = tmp_dir("concurrent-same");
        let sends = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (dir, sends, barrier, id) =
                    (dir.clone(), sends.clone(), barrier.clone(), id.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    guarded(
                        &dir,
                        &id,
                        &sends,
                        Err(NodeError::PersistenceFailed),
                        Duration::from_millis(300),
                    )
                })
            })
            .collect();
        let res: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(sends.load(Ordering::SeqCst), 1, "{res:?}");
        assert!(
            res.iter().all(|r| *r == Err(LnError::PaymentStateUnknown)),
            "{res:?}"
        );
        // The owner's record survives.
        assert_eq!(Attempts::load(&dir).unwrap().get(&id), Some(2_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_pays_of_different_hashes_both_send() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Barrier};
        let dir = tmp_dir("concurrent-diff");
        let sends = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["11", "22"]
            .into_iter()
            .map(|b| {
                let (dir, sends, barrier) = (dir.clone(), sends.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let id = b.repeat(32); // "11"/"22": used by no other test
                    barrier.wait();
                    guarded(
                        &dir,
                        &id,
                        &sends,
                        Ok(PaymentId([0; 32])),
                        Duration::from_millis(300),
                    )
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
        assert_eq!(sends.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn guarded_section_checks_then_records() {
        use std::sync::atomic::AtomicU32;
        let dir = tmp_dir("guarded-wiring");
        let sends = AtomicU32::new(0);
        // The check runs: an unknown attempt blocks the send.
        let _ = send_recorded(&dir, ID, 2_000, 1_000, || Err(NodeError::PersistenceFailed));
        assert_eq!(
            guarded(&dir, ID, &sends, Ok(PaymentId([0xab; 32])), Duration::ZERO),
            Err(LnError::PaymentStateUnknown)
        );
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The record is written before the send.
        let other = "cd".repeat(32);
        let r = guarded_bolt11(
            &dir,
            &other,
            2_000,
            1_000,
            || false,
            || Ok(()),
            || {
                assert_eq!(Attempts::load(&dir).unwrap().get(&other), Some(2_000));
                Err(NodeError::PersistenceFailed)
            },
        );
        assert_eq!(r, Err(LnError::PaymentStateUnknown));
        // A failing pre-send step sends nothing and records nothing.
        let third = "ef".repeat(32);
        let r = guarded_bolt11(
            &dir,
            &third,
            2_000,
            1_000,
            || false,
            || Err(LnError::BackupUnavailable),
            || panic!("must not send"),
        );
        assert_eq!(r, Err(LnError::BackupUnavailable));
        assert_eq!(Attempts::load(&dir).unwrap().get(&third), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicate_payment_leaves_the_record() {
        let dir = tmp_dir("attempt-dup");
        let r = send_recorded(&dir, ID, 2_000, 1_000, || Err(NodeError::DuplicatePayment));
        assert_eq!(r, Err(LnError::Other("duplicate_payment".into())));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_unknown_writes_a_missing_record_back() {
        let dir = tmp_dir("attempt-writeback");
        let r = send_recorded(&dir, ID, 2_000, 1_000, || {
            // Something removed it while the send was running.
            Attempts::update(&dir, 1_000, |a| {
                a.remove(ID);
            })
            .unwrap();
            Err(NodeError::PersistenceFailed)
        });
        assert_eq!(r, Err(LnError::PaymentStateUnknown));
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_attempts_file_refuses_and_is_left_alone() {
        let dir = tmp_dir("attempt-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ATTEMPTS_FILE);
        std::fs::write(&path, b"\x00garbage{").unwrap();
        let refused = LnError::Other("attempt_record_unreadable".into());
        assert_eq!(
            check_attempt(&dir, ID, 1_000, || false),
            Err(refused.clone())
        );
        assert_eq!(
            send_recorded(&dir, ID, 2_000, 1_000, || panic!("must not send")),
            Err(refused)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"\x00garbage{");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_attempts_file_refuses() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("attempt-unreadable");
        let _ = send_recorded(&dir, ID, 2_000, 1_000, || Err(NodeError::PersistenceFailed));
        let path = dir.join(ATTEMPTS_FILE);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            eprintln!("skipping: running with read access to mode-000 files");
        } else {
            let refused = LnError::Other("attempt_record_unreadable".into());
            assert_eq!(
                check_attempt(&dir, "11".repeat(32).as_str(), 1_000, || false),
                Err(refused.clone())
            );
            assert_eq!(
                send_recorded(&dir, &"11".repeat(32), 2_000, 1_000, || panic!(
                    "must not send"
                )),
                Err(refused)
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(Attempts::load(&dir).unwrap().get(ID), Some(2_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn notes_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("fortis-pay-unit-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Notes::update(&dir, |n| {
            n.insert("aa".into(), "coffee".into());
        });
        Notes::update(&dir, |n| {
            n.insert("bb".into(), "tea".into());
        });
        let n = Notes::load(&dir);
        assert_eq!(n.0.get("aa").map(String::as_str), Some("coffee"));
        assert_eq!(n.0.get("bb").map(String::as_str), Some("tea"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Task 19: the local record files are read from disk and may hold anything
/// (a crash mid-write elsewhere, a full disk, tampering on a rooted phone).
#[cfg(test)]
mod fuzz {
    use super::*;
    use proptest::prelude::*;

    fn unique_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "fortis-pay-fuzz-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

        /// Notes and attempts readers never panic on arbitrary bytes. A
        /// corrupt attempts file fails CLOSED: the payment is refused, `send`
        /// is never called, and the file is left exactly as it was.
        #[test]
        fn record_readers_survive_arbitrary_bytes_and_a_bad_attempts_file_blocks_sending(
            bytes in proptest::collection::vec(any::<u8>(), 0..2000),
        ) {
            let dir = unique_dir();
            std::fs::write(dir.join(NOTES_FILE), &bytes).unwrap();
            std::fs::write(dir.join(ATTEMPTS_FILE), &bytes).unwrap();

            let _ = Notes::read(&dir); // must not panic

            let parses = serde_json::from_slice::<BTreeMap<String, u64>>(&bytes).is_ok();
            prop_assert_eq!(Attempts::read(&dir).is_ok(), parses);
            if !parses {
                prop_assert_eq!(
                    check_attempt(&dir, "id", 0, || true),
                    Err(LnError::Other("attempt_record_unreadable".into()))
                );
                let sent = send_recorded(&dir, "id", 100, 0, || -> Result<PaymentId, NodeError> {
                    unreachable!("a payment was sent past a corrupt attempts file")
                });
                prop_assert!(sent.is_err());
                prop_assert_eq!(&std::fs::read(dir.join(ATTEMPTS_FILE)).unwrap(), &bytes);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
