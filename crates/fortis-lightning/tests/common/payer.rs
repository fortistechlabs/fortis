//! A stock ldk-node that pays invoices through the harness LSP: funded on
//! regtest, with an open (confirmed, usable) channel to the [`Lsp`], so it
//! has outbound liquidity through it.

use std::str::FromStr;

use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning_invoice::Bolt11Invoice;
use ldk_node::payment::PaymentStatus;
use ldk_node::{Event, Node};

use super::{builder, poll, test_seed, wait_event, Lsp, Regtest, TempDir};

/// On-chain funding of the payer (sat).
pub const PAYER_FUNDING_SAT: u64 = 2_000_000;
/// The payer → LSP channel (sat).
pub const PAYER_CHANNEL_SAT: u64 = 1_000_000;

/// The payer node; stopped on drop. Fields drop in order: node, then dir.
pub struct Payer {
    pub node: Node,
    _dir: TempDir,
}

impl Payer {
    /// Builds and starts a payer, funds it, opens a [`PAYER_CHANNEL_SAT`]
    /// channel to `lsp`, mines it and waits until the channel is usable.
    pub fn start(rt: &Regtest, lsp: &Lsp) -> Payer {
        let dir = TempDir::new("payer");
        let (b, _addr) = builder(rt, test_seed(0x5A), &dir.str("payer"));
        let node = b.build_with_fs_store().unwrap();
        node.start().unwrap();
        let payer = Payer { node, _dir: dir };

        let addr = payer.node.onchain_payment().new_address().unwrap();
        rt.fund(&addr.to_string(), PAYER_FUNDING_SAT);
        rt.mine(6);
        poll("payer sees its funding", 90, || {
            payer.node.sync_wallets().ok();
            (payer.node.list_balances().spendable_onchain_balance_sats > 0).then_some(())
        });

        let lsp_id = PublicKey::from_str(&lsp.node_id).unwrap();
        let lsp_addr: SocketAddress = format!("{}:{}", lsp.host, lsp.port).parse().unwrap();
        payer
            .node
            .open_channel(lsp_id, lsp_addr, PAYER_CHANNEL_SAT, None, None)
            .expect("payer opens a channel to the LSP");
        wait_event(&payer.node, "payer channel pending", 60, |e| {
            matches!(e, Event::ChannelPending { .. })
        });
        rt.mine(6);
        wait_event(&payer.node, "payer channel ready", 90, |e| {
            matches!(e, Event::ChannelReady { .. })
        });
        poll("payer channel usable", 60, || {
            payer
                .node
                .list_channels()
                .iter()
                .any(|c| c.counterparty_node_id == lsp_id && c.is_usable)
                .then_some(())
        });
        payer
    }

    /// Pays `invoice` and waits until the payment succeeded.
    pub fn pay(&self, invoice: &str) {
        let inv = Bolt11Invoice::from_str(invoice).expect("a bolt11 invoice");
        let id = self
            .node
            .bolt11_payment()
            .send(&inv, None)
            .expect("payer sends");
        wait_event(&self.node, "payer payment successful", 120, |e| match e {
            Event::PaymentSuccessful { payment_id, .. } => *payment_id == Some(id),
            Event::PaymentFailed {
                payment_id, reason, ..
            } if *payment_id == Some(id) => {
                panic!("payment failed: {reason:?}")
            }
            _ => false,
        });
        assert_eq!(
            self.node.payment(&id).map(|p| p.status),
            Some(PaymentStatus::Succeeded)
        );
    }
}

impl Drop for Payer {
    fn drop(&mut self) {
        let _ = self.node.stop();
    }
}
