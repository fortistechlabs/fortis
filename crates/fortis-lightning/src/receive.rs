//! [`Node::receive`]: a BOLT11 invoice for an amount, with the LSP's
//! channel-open fee quoted up front when receiving it needs a new (JIT)
//! channel.
//!
//! **Plain or JIT.** When the node's usable channels have at least the
//! amount in inbound capacity, the invoice is a plain one (fee 0). Otherwise
//! ldk-node's LSPS2 client asks the LSP for its fee menu (`get_info`), picks
//! the cheapest offer for the amount, `buy`s it and returns an invoice with
//! the LSP's intercept route hint: the payer pays the full amount, the LSP
//! opens a zero-conf channel and forwards the amount less the fee. The fee
//! ldk-node agreed to (and will enforce as the most the LSP may skim) is read
//! back from the payment it stored for the invoice, so it is known before the
//! invoice is shown: "fee X, you get amount − X".
//!
//! **Range.** Zero is always out of range. A JIT receive must lie within the
//! LSP's payment bounds ([`Node::limits`]); a receive over existing inbound
//! capacity has no LSPS2 bounds (the LSP only forwards).

use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
use ldk_node::payment::PaymentKind;
use ldk_node::NodeError;

use crate::{LnError, Node};

/// How long a receive invoice stays payable.
const INVOICE_EXPIRY_SECS: u32 = 3_600;

/// What [`Node::receive`] returns: the invoice to show and its cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveQuote {
    /// BOLT11, for the full `amount_msat`.
    pub invoice: String,
    /// What the payer pays.
    pub amount_msat: u64,
    /// The LSP's channel-open fee, deducted from what arrives; 0 when no
    /// channel is needed. The node receives `amount_msat - fee_msat`.
    pub fee_msat: u64,
    /// Paying this invoice opens a JIT channel from the LSP.
    pub needs_channel: bool,
}

impl Node {
    /// An invoice for `amount_msat` with `description` (at most 639 bytes).
    ///
    /// A plain invoice when the usable channels' inbound covers the amount
    /// with headroom (see `plain_receive_fits`), otherwise a JIT one.
    ///
    /// **Blocking.** On the JIT path this performs a real LSPS2 `get_info` +
    /// `buy` with the LSP, which can take up to ~10 s: call it off the UI
    /// thread, and once per confirmed amount, never per keystroke.
    ///
    /// The invoice expires after 3 600 s. That is not tied to the validity of
    /// the LSP's fee parameters (`valid_until`); it is safe as long as those
    /// outlive the invoice, as fortis-lsp's (24 h) do.
    ///
    /// Errors: `Other("not_started")` before [`start`](Node::start);
    /// `AmountOutOfRange` (with the LSP limits) for 0, or for a JIT receive
    /// outside them; `LspUnreachable` when the LSP does not answer the LSPS2
    /// request; `BackupUnavailable` when the payment cannot be persisted.
    /// On a node started in [`StartMode::Exit`](crate::StartMode::Exit):
    /// `LspUnreachable` right after `not_started`, at once, while the LSP is
    /// not connected.
    pub fn receive(&self, amount_msat: u64, description: &str) -> Result<ReceiveQuote, LnError> {
        let running = self
            .running()
            .ok_or_else(|| LnError::Other("not_started".into()))?;
        // Exit mode, LSP not connected: refused before anything, plain
        // invoice included (nothing could be forwarded to us anyway).
        self.exit_gate(&running)?;
        let node = &running.node;
        let limits = self.limits();
        let out_of_range = LnError::AmountOutOfRange {
            min_msat: limits.min_payment_msat,
            max_msat: limits.max_payment_msat,
        };
        if amount_msat == 0 {
            return Err(out_of_range);
        }
        let text = description;
        let description = Description::new(description.to_string())
            .map(Bolt11InvoiceDescription::Direct)
            .map_err(|_| LnError::Other("description_too_long".into()))?;

        let channels = node.list_channels();
        let usable: Vec<_> = channels.iter().filter(|c| c.is_usable).collect();
        let inbound_msat: u64 = usable.iter().map(|c| c.inbound_capacity_msat).sum();
        let capacity_msat: u64 = usable
            .iter()
            .map(|c| c.channel_value_sats.saturating_mul(1_000))
            .sum();
        // The largest single HTLC any usable channel takes; unknown (`None`)
        // when there is no channel or one of them does not say.
        let htlc_max_msat = usable
            .iter()
            .map(|c| c.inbound_htlc_maximum_msat)
            .try_fold(0u64, |m, h| h.map(|h| m.max(h)))
            .filter(|_| !usable.is_empty());
        let needs_channel =
            !plain_receive_fits(inbound_msat, capacity_msat, amount_msat, htlc_max_msat);

        if !needs_channel {
            self.pre_send_checks()?;
            let invoice = node
                .bolt11_payment()
                .receive(amount_msat, &description, INVOICE_EXPIRY_SECS)
                .map_err(map_error)?;
            self.note_description(&hex::encode(invoice.payment_hash().as_ref() as &[u8]), text);
            return Ok(ReceiveQuote {
                invoice: invoice.to_string(),
                amount_msat,
                fee_msat: 0,
                needs_channel: false,
            });
        }

        if running.mode == crate::StartMode::Exit {
            return Err(LnError::Other("lightning_disabled".into()));
        }
        if amount_msat < limits.min_payment_msat || amount_msat > limits.max_payment_msat {
            return Err(out_of_range);
        }
        self.pre_send_checks()?;
        let invoice = node
            .bolt11_payment()
            .receive_via_jit_channel(amount_msat, &description, INVOICE_EXPIRY_SECS, None)
            .map_err(map_error)?;
        // ldk-node stores the JIT payment (id = payment hash) with the total
        // fee it accepted as the LSP's skim limit.
        let id = PaymentId(*invoice.payment_hash().as_ref());
        self.note_description(&hex::encode(id.0), text);
        let fee_msat = match node.payment(&id).map(|p| p.kind) {
            Some(PaymentKind::Bolt11Jit { lsp_fee_limits, .. }) => lsp_fee_limits
                .max_total_opening_fee_msat
                .ok_or_else(|| LnError::Other("jit fee unknown".into()))?,
            _ => return Err(LnError::Other("jit payment not stored".into())),
        };
        Ok(ReceiveQuote {
            invoice: invoice.to_string(),
            amount_msat,
            fee_msat,
            needs_channel: true,
        })
    }
}

/// Least headroom kept under the reported inbound capacity (sat).
const HEADROOM_FLOOR_SAT: u64 = 5_000;
/// Headroom as a share of the counted channels' capacity: 1 / 100.
const HEADROOM_DIVISOR: u64 = 100;

/// Headroom (msat) kept under the reported inbound capacity:
/// max([`HEADROOM_FLOOR_SAT`], 1 % of `capacity_msat`).
fn headroom_msat(capacity_msat: u64) -> u64 {
    (HEADROOM_FLOOR_SAT * 1_000).max(capacity_msat / HEADROOM_DIVISOR)
}

/// Whether a receive of `amount_msat` can be a plain invoice over the
/// existing channels, rather than a JIT one.
///
/// `inbound_msat` is LDK's `inbound_capacity_msat` summed over the usable
/// channels. It is nominal: on a channel the LSP funded, the LSP pays the
/// commitment fee and anchor outputs out of its side, and each extra HTLC
/// raises that fee, so the LSP cannot forward the full reported amount (and
/// the client cannot see the LSP's `next_outbound_htlc_limit_msat`). So the
/// plain path needs `amount + headroom <= inbound`, with
/// headroom = max(5 000 sat, 1 % of `capacity_msat`, the counted channels'
/// total size). When `htlc_max_msat` (the largest `inbound_htlc_maximum_msat`
/// of those channels) is known, the amount must also fit in one HTLC.
/// Erring towards JIT costs a quoted fee for a larger channel; erring
/// the other way is an invoice the payer cannot pay.
fn plain_receive_fits(
    inbound_msat: u64,
    capacity_msat: u64,
    amount_msat: u64,
    htlc_max_msat: Option<u64>,
) -> bool {
    if htlc_max_msat.is_some_and(|max| amount_msat > max) {
        return false;
    }
    amount_msat
        .checked_add(headroom_msat(capacity_msat))
        .is_some_and(|need| need <= inbound_msat)
}

fn map_error(e: NodeError) -> LnError {
    match e {
        NodeError::NotRunning => LnError::Other("not_started".into()),
        NodeError::ConnectionFailed | NodeError::LiquidityRequestFailed => LnError::LspUnreachable,
        NodeError::PersistenceFailed => LnError::BackupUnavailable,
        other => LnError::Other(format!("receive: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAT: u64 = 1_000;

    #[test]
    fn exact_boundary() {
        // 198 000 sat channel: 1 % = 1 980 sat < the 5 000 sat floor.
        let cap = 198_000 * SAT;
        let inbound = 97_020 * SAT;
        let edge = inbound - 5_000 * SAT;
        assert!(plain_receive_fits(inbound, cap, edge, None));
        assert!(!plain_receive_fits(inbound, cap, edge + 1, None));
        assert!(!plain_receive_fits(inbound, cap, inbound, None));
    }

    #[test]
    fn htlc_max_below_amount_is_jit() {
        let (cap, inbound) = (1_000_000 * SAT, 900_000 * SAT);
        let amount = 100_000 * SAT;
        assert!(plain_receive_fits(inbound, cap, amount, Some(amount)));
        assert!(!plain_receive_fits(inbound, cap, amount, Some(amount - 1)));
    }

    #[test]
    fn no_channel_is_jit() {
        assert!(!plain_receive_fits(0, 0, 1, None));
        assert!(!plain_receive_fits(0, 0, 50_000 * SAT, None));
    }

    #[test]
    fn headroom_floor_and_percentage() {
        assert_eq!(headroom_msat(0), 5_000 * SAT);
        assert_eq!(headroom_msat(198_000 * SAT), 5_000 * SAT, "floor beats 1 %");
        assert_eq!(headroom_msat(500_000 * SAT), 5_000 * SAT, "1 % == floor");
        assert_eq!(
            headroom_msat(2_000_000 * SAT),
            20_000 * SAT,
            "1 % beats floor"
        );
        // Percentage case at the boundary.
        let (cap, inbound) = (2_000_000 * SAT, 1_000_000 * SAT);
        assert!(plain_receive_fits(
            inbound,
            cap,
            inbound - 20_000 * SAT,
            None
        ));
        assert!(!plain_receive_fits(
            inbound,
            cap,
            inbound - 20_000 * SAT + 1,
            None
        ));
    }

    #[test]
    fn no_overflow() {
        assert!(!plain_receive_fits(u64::MAX, u64::MAX, u64::MAX, None));
        let _ = headroom_msat(u64::MAX);
    }
}
