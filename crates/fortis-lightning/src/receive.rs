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
    /// Errors: `Other("not_started")` before [`start`](Node::start);
    /// `AmountOutOfRange` (with the LSP limits) for 0, or for a JIT receive
    /// outside them; `LspUnreachable` when the LSP does not answer the LSPS2
    /// request; `BackupUnavailable` when the payment cannot be persisted.
    pub fn receive(&self, amount_msat: u64, description: &str) -> Result<ReceiveQuote, LnError> {
        let running = self
            .running()
            .ok_or_else(|| LnError::Other("not_started".into()))?;
        let node = &running.node;
        let limits = self.limits();
        let out_of_range = LnError::AmountOutOfRange {
            min_msat: limits.min_payment_msat,
            max_msat: limits.max_payment_msat,
        };
        if amount_msat == 0 {
            return Err(out_of_range);
        }
        let description = Description::new(description.to_string())
            .map(Bolt11InvoiceDescription::Direct)
            .map_err(|_| LnError::Other("description_too_long".into()))?;

        let channels = node.list_channels();
        let usable = channels.iter().filter(|c| c.is_usable);
        let inbound_msat: u64 = usable.map(|c| c.inbound_capacity_msat).sum();
        let needs_channel = inbound_msat < amount_msat;

        if !needs_channel {
            let invoice = node
                .bolt11_payment()
                .receive(amount_msat, &description, INVOICE_EXPIRY_SECS)
                .map_err(map_error)?;
            return Ok(ReceiveQuote {
                invoice: invoice.to_string(),
                amount_msat,
                fee_msat: 0,
                needs_channel: false,
            });
        }

        if amount_msat < limits.min_payment_msat || amount_msat > limits.max_payment_msat {
            return Err(out_of_range);
        }
        let invoice = node
            .bolt11_payment()
            .receive_via_jit_channel(amount_msat, &description, INVOICE_EXPIRY_SECS, None)
            .map_err(map_error)?;
        // ldk-node stores the JIT payment (id = payment hash) with the total
        // fee it accepted as the LSP's skim limit.
        let id = PaymentId(*invoice.payment_hash().as_ref());
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

fn map_error(e: NodeError) -> LnError {
    match e {
        NodeError::NotRunning => LnError::Other("not_started".into()),
        NodeError::ConnectionFailed | NodeError::LiquidityRequestFailed => LnError::LspUnreachable,
        NodeError::PersistenceFailed => LnError::BackupUnavailable,
        other => LnError::Other(format!("receive: {other}")),
    }
}
