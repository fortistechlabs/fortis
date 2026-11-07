//! The Fortis account API: `POST /account` on the loopback metrics listener
//! (reached through the edge's `/ln/account`), and the LDK-event hooks that
//! credit and settle accounts. See `account.rs` (ledger) and
//! `account_auth.rs` (signed requests).
//!
//! Ops: `balance`, `invoice {amount_msat, description}`,
//! `pay {invoice, amount_msat?}`, `history {limit}`.

use std::sync::Arc;

use ldk_node::bitcoin::secp256k1::PublicKey;
use serde_json::{json, Value};

use crate::account::{Ledger, LedgerError};
use crate::account_auth::{self, AuthError};

/// How long an account invoice is valid.
pub const INVOICE_EXPIRY_SECS: u32 = 3600;
/// Largest request body.
pub const MAX_BODY: usize = 16 * 1024;
/// Most history entries one request returns.
pub const MAX_HISTORY: usize = 200;

/// The routing-fee reserve held for an account send: the wallet's own cap,
/// The largest transfer-fee invoice (`fee_invoice`): 1 % of a 5,000,000 sat transfer.
pub const MAX_FEE_INVOICE_MSAT: u64 = 50_000_000;

/// The withdrawal transaction's size for the fee estimate (1 input, 2 outputs, with margin).
pub const WITHDRAW_TX_VBYTES: u64 = 200;
/// The highest fee rate a withdrawal accepts.
pub const MAX_FEERATE_SAT_VB: u64 = 500;
/// The smallest output worth creating.
pub const DUST_SAT: u64 = 546;

/// A withdrawal of `amount_sat` (everything leaving the account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Withdrawal {
    pub payout_sat: u64,
    /// Network fee plus the operator's share (none below dust).
    pub fee_sat: u64,
}

/// The app's rule (`Transfer.kt`): fee `max(1 %, 1,000 sat, network fee)`,
/// the operator's share (fee − network fee) dropped below dust. `None` when
/// less than dust would arrive.
pub fn withdrawal(amount_sat: u64, feerate_sat_vb: u64) -> Option<Withdrawal> {
    let net = WITHDRAW_TX_VBYTES * feerate_sat_vb.max(1);
    let fee = (amount_sat / 100).max(1_000).max(net);
    let share = if fee - net < DUST_SAT { 0 } else { fee - net };
    let fee_sat = net + share;
    amount_sat
        .checked_sub(fee_sat)
        .filter(|p| *p >= DUST_SAT)
        .map(|payout_sat| Withdrawal { payout_sat, fee_sat })
}

/// 1 % + 50 sat.
pub fn fee_cap_msat(amount_msat: u64) -> u64 {
    amount_msat / 100 + 50_000
}

/// What the account service needs from the node (a trait so the handlers
/// can be tested without one).
pub trait AccountNode: Send + Sync {
    /// True while `node`'s phone is connected to us (then its app shows a receipt itself).
    fn is_connected(&self, _node: &PublicKey) -> bool {
        false
    }
    /// A BOLT11 invoice paying this node; its payment hash.
    fn receive_invoice(
        &self,
        amount_msat: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<(String, [u8; 32]), String>;
    /// Parses a BOLT11 invoice: its amount (if any) and payment hash.
    fn invoice_details(&self, invoice: &str) -> Result<(Option<u64>, [u8; 32]), String>;
    /// Starts paying `invoice` (`amount_msat` for an amountless one).
    fn send_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<(), String>;
    /// What the node could pay out now: spendable on-chain + outbound (sat).
    fn backing_sat(&self) -> u64;
    /// Sends `amount_sat` on-chain to `address` at `feerate_sat_vb`; the txid.
    /// `Err(None)`: the address is not one of this network's.
    fn send_onchain(&self, address: &str, amount_sat: u64, feerate_sat_vb: u64) -> Result<[u8; 32], Option<String>>;
    /// An invoice for `payment_hash` that this node settles only when told
    /// (held payments, `held.rs`).
    fn receive_for_hash(&self, amount_msat: u64, description: &str, expiry_secs: u32, payment_hash: [u8; 32]) -> Result<String, String>;
    /// A new reusable, any-amount BOLT12 offer paying this node; its id.
    fn create_offer(&self, description: &str) -> Result<(String, [u8; 32]), String>;
    /// The offer an inbound payment paid, if it was a BOLT12 offer payment.
    fn paid_offer(&self, payment_id: [u8; 32]) -> Option<[u8; 32]>;
}

/// FCM registration tokens are ~160 characters; anything far longer is not one.
pub const MAX_PUSH_TOKEN_LEN: usize = 1024;

pub struct AccountService {
    pub ledger: Arc<Ledger>,
    node: Arc<dyn AccountNode>,
    /// Per-account cap (msat): the account converts above it (`conversion.rs`).
    pub account_max_msat: u64,
    /// All accounts together (msat); 0 = accounts disabled.
    pub custody_max_msat: u64,
    /// The smallest receive that opens a channel instead (sat), for the
    /// "custody full" reply.
    pub min_channel_receive_sat: u64,
    /// Push notifications for payments held while a phone is offline.
    pub fcm: Option<crate::push::Fcm>,
    /// `btc` or `xbt`: named in push data.
    pub chain: &'static str,
    /// Receipt notices decided on (sent, or only logged without push), for tests and logs.
    notices: std::sync::atomic::AtomicU64,
}

/// An HTTP status and JSON body.
pub type Reply = (u16, Value);

fn err(status: u16, error: &str) -> Reply {
    (status, json!({ "error": error }))
}

fn send_hold_id(payment_hash: &[u8; 32]) -> String {
    format!("send:{}", hex::encode(payment_hash))
}

impl AccountService {
    pub fn new(
        ledger: Arc<Ledger>,
        node: Arc<dyn AccountNode>,
        account_max_sat: u64,
        custody_max_sat: u64,
        min_channel_receive_sat: u64,
        chain: crate::config::Chain,
    ) -> AccountService {
        AccountService {
            ledger,
            node,
            account_max_msat: account_max_sat.saturating_mul(1_000),
            custody_max_msat: custody_max_sat.saturating_mul(1_000),
            min_channel_receive_sat,
            fcm: None,
            chain: chain.as_str(),
            notices: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn with_push(mut self, fcm: Option<crate::push::Fcm>) -> AccountService {
        self.fcm = fcm;
        self
    }

    /// How many receipt notices were decided on since start.
    pub fn notices_sent(&self) -> u64 {
        self.notices.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn custody_sat(&self) -> u64 {
        self.ledger.total_custody_msat().unwrap_or(0) / 1_000
    }

    /// Handles one `POST /account`.
    pub fn handle(&self, body: &[u8], sig: Option<&str>, now: u64) -> Reply {
        if body.len() > MAX_BODY {
            return err(413, "too_large");
        }
        let Some(sig) = sig else {
            return (
                401,
                json!({ "error": "auth", "reason": "missing_signature" }),
            );
        };
        let req = match account_auth::verify(body, sig, now, &self.ledger) {
            Ok(r) => r,
            Err(e) => {
                let status = if e == AuthError::Malformed { 400 } else { 401 };
                return (status, json!({ "error": "auth", "reason": e.as_str() }));
            }
        };
        let node = req.node_id;
        let r = match req.op.as_str() {
            "balance" => self.balance(&node),
            "invoice" => self.invoice(&node, &req.params, now),
            "pay" => self.pay(&node, &req.params),
            "history" => self.history(&node, &req.params),
            "fee_invoice" => Ok(self.fee_invoice(&node, &req.params)),
            "withdraw" => self.withdraw(&node, &req.params, &req.nonce),
            "offer" => self.offer(&node),
            "channel_invoice" => self.channel_invoice(&node, &req.params, now),
            "push_register" => self.push_register(&node, &req.params, now),
            _ => Ok(err(400, "unknown_op")),
        };
        r.unwrap_or_else(|e| {
            eprintln!("fortis-lsp: account {}: {e}", req.op);
            err(500, "ledger")
        })
    }

    fn balance(&self, node: &PublicKey) -> Result<Reply, LedgerError> {
        let a = self.ledger.balance(node)?;
        Ok((
            200,
            json!({ "balance_msat": a.balance_msat, "held_msat": a.held_msat, "max_msat": self.account_max_msat }),
        ))
    }

    fn custody_full(&self) -> Reply {
        (
            409,
            json!({ "error": "custody_full", "min_channel_sat": self.min_channel_receive_sat }),
        )
    }

    fn invoice(&self, node: &PublicKey, p: &Value, now: u64) -> Result<Reply, LedgerError> {
        let Some(amount) = p["amount_msat"].as_u64().filter(|a| *a > 0) else {
            return Ok(err(400, "amount_msat"));
        };
        let description = p["description"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>();
        if self.custody_max_msat == 0 {
            return Ok(self.custody_full());
        }
        let after = self.ledger.total_custody_msat()?.saturating_add(amount);
        if after > self.custody_max_msat || after / 1_000 > self.node.backing_sat() {
            return Ok(self.custody_full());
        }
        let (invoice, hash) =
            match self
                .node
                .receive_invoice(amount, &description, INVOICE_EXPIRY_SECS)
            {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("fortis-lsp: account invoice: {e}");
                    return Ok(err(502, "node"));
                }
            };
        self.ledger
            .pending_invoice(hash, node, amount, now + INVOICE_EXPIRY_SECS as u64)?;
        Ok((200, json!({ "invoice": invoice })))
    }

    /// An invoice for the operator's share of a transfer fee (spec §2): paid
    /// to the LSP and credited to no account.
    fn fee_invoice(&self, node: &PublicKey, p: &Value) -> Reply {
        let Some(amount) = p["amount_msat"]
            .as_u64()
            .filter(|a| (1..=MAX_FEE_INVOICE_MSAT).contains(a))
        else {
            return err(400, "amount_msat");
        };
        match self
            .node
            .receive_invoice(amount, "Fortis transfer fee", INVOICE_EXPIRY_SECS)
        {
            Ok((invoice, hash)) => {
                // Known as the operator's revenue when it is paid.
                if let Err(e) = self.ledger.add_fee_invoice(hash, node, amount) {
                    eprintln!("fortis-lsp: fee invoice not recorded: {e}");
                }
                eprintln!("fortis-lsp: transfer fee invoice of {amount} msat for {node}");
                (200, json!({ "invoice": invoice }))
            }
            Err(e) => {
                eprintln!("fortis-lsp: fee invoice: {e}");
                err(502, "node")
            }
        }
    }

    /// An invoice paying into the phone's own channel, held by the LSP until
    /// the phone takes it (`held.rs`).
    fn channel_invoice(&self, node: &PublicKey, p: &Value, _now: u64) -> Result<Reply, LedgerError> {
        let Some(amount) = p["amount_msat"].as_u64().filter(|a| *a > 0) else {
            return Ok(err(400, "amount_msat"));
        };
        let description = p["description"].as_str().unwrap_or("").chars().take(200).collect::<String>();
        let preimage = crate::held::new_preimage();
        let hash = crate::held::hash_of(&preimage);
        match self.node.receive_for_hash(amount, &description, INVOICE_EXPIRY_SECS, hash) {
            Ok(invoice) => {
                self.ledger.held_insert(hash, node, preimage, amount)?;
                Ok((200, json!({ "invoice": invoice })))
            }
            Err(e) => {
                eprintln!("fortis-lsp: channel invoice: {e}");
                Ok(err(502, "node"))
            }
        }
    }

    /// The account's reusable BOLT12 offer (created once): payments to it are
    /// credited to the account even while the phone is off.
    /// Registers the phone's push token (`params.token`, a Firebase Cloud
    /// Messaging registration token) so a payment held while it is offline
    /// can wake it.
    fn push_register(&self, node: &PublicKey, p: &Value, now: u64) -> Result<Reply, LedgerError> {
        let Some(token) = p["token"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= MAX_PUSH_TOKEN_LEN && t.bytes().all(|b| b.is_ascii_graphic()))
        else {
            return Ok(err(400, "token"));
        };
        self.ledger.set_push_token(node, token, now)?;
        Ok((200, json!({ "ok": true })))
    }

    fn offer(&self, node: &PublicKey) -> Result<Reply, LedgerError> {
        if self.custody_max_msat == 0 {
            return Ok(self.custody_full());
        }
        if let Some(offer) = self.ledger.offer_of(node)? {
            return Ok((200, json!({ "offer": offer })));
        }
        match self.node.create_offer("Fortis") {
            Ok((offer, id)) => {
                self.ledger.set_offer(node, id, &offer)?;
                Ok((200, json!({ "offer": offer })))
            }
            Err(e) => {
                eprintln!("fortis-lsp: account offer: {e}");
                Ok(err(502, "node"))
            }
        }
    }

    /// Moves `amount_msat` (everything leaving the account) to an on-chain
    /// `address`: the LSP sends `amount − fee` from its wallet, fee
    /// `max(1 %, 1,000 sat, network fee)` with the network fee estimated as
    /// [`WITHDRAW_TX_VBYTES`] at the caller's `feerate_sat_vb` (spec §2).
    fn withdraw(&self, node: &PublicKey, p: &Value, nonce: &str) -> Result<Reply, LedgerError> {
        let (Some(amount_msat), Some(address), Some(feerate)) = (
            p["amount_msat"].as_u64().filter(|a| *a > 0 && a % 1_000 == 0),
            p["address"].as_str(),
            p["feerate_sat_vb"].as_u64().filter(|f| (1..=MAX_FEERATE_SAT_VB).contains(f)),
        ) else {
            return Ok(err(400, "params"));
        };
        let Some(w) = withdrawal(amount_msat / 1_000, feerate) else {
            return Ok(err(400, "too_small"));
        };
        let id = format!("wd:{node}:{nonce}");
        match self.ledger.hold_withdrawal(node, &id, amount_msat) {
            Ok(()) => {}
            Err(LedgerError::InsufficientBalance) => return Ok(err(402, "insufficient")),
            Err(e) => return Err(e),
        }
        match self.node.send_onchain(address, w.payout_sat, feerate) {
            Ok(txid) => {
                self.ledger.settle_withdrawal(&id, w.fee_sat * 1_000, txid)?;
                let txid_hex = hex::encode(txid.iter().rev().copied().collect::<Vec<u8>>());
                eprintln!("fortis-lsp: account withdrawal of {} sat for {node}: {txid_hex}", w.payout_sat);
                Ok((200, json!({ "txid": txid_hex, "payout_sat": w.payout_sat, "fee_sat": w.fee_sat })))
            }
            Err(e) => {
                self.ledger.settle_hold(&id, 0, 0, None)?;
                Ok(match e {
                    None => err(400, "address"),
                    Some(e) => {
                        eprintln!("fortis-lsp: account withdrawal for {node}: {e}");
                        err(502, "node")
                    }
                })
            }
        }
    }

    fn pay(&self, node: &PublicKey, p: &Value) -> Result<Reply, LedgerError> {
        let Some(invoice) = p["invoice"].as_str() else {
            return Ok(err(400, "invoice"));
        };
        let (inv_amount, hash) = match self.node.invoice_details(invoice) {
            Ok(v) => v,
            Err(_) => return Ok(err(400, "invoice")),
        };
        let asked = p["amount_msat"].as_u64();
        let amount = match (inv_amount, asked) {
            (Some(a), None) => a,
            (Some(a), Some(b)) if a == b => a,
            (Some(_), Some(_)) => return Ok(err(400, "amount_mismatch")),
            (None, Some(b)) if b > 0 => b,
            _ => return Ok(err(400, "amount_required")),
        };
        // Another Fortis account's invoice: settle it in the ledger.
        if let Some(to) = self.ledger.invoice_owner(hash)? {
            if &to == node {
                return Ok(err(400, "own_invoice"));
            }
            return match self.ledger.transfer(node, &to, hash, amount) {
                Ok(_) => Ok((
                    200,
                    json!({ "internal": true, "payment_hash": hex::encode(hash), "fee_msat": 0 }),
                )),
                Err(LedgerError::InsufficientBalance) => Ok(err(402, "insufficient")),
                Err(e) => Err(e),
            };
        }
        let id = send_hold_id(&hash);
        match self
            .ledger
            .hold_send(node, &id, amount, amount + fee_cap_msat(amount))
        {
            Ok(()) => {}
            Err(LedgerError::InsufficientBalance) => return Ok(err(402, "insufficient")),
            Err(e) => return Err(e),
        }
        let pay_amount = if inv_amount.is_none() {
            Some(amount)
        } else {
            None
        };
        if let Err(e) = self.node.send_bolt11(invoice, pay_amount) {
            eprintln!("fortis-lsp: account pay: {e}");
            self.ledger.settle_hold(&id, 0, 0, None)?;
            return Ok(err(502, "send_failed"));
        }
        Ok((
            200,
            json!({ "internal": false, "payment_hash": hex::encode(hash) }),
        ))
    }

    fn history(&self, node: &PublicKey, p: &Value) -> Result<Reply, LedgerError> {
        let limit = p["limit"]
            .as_u64()
            .map_or(50, |l| l as usize)
            .clamp(1, MAX_HISTORY);
        let entries: Vec<Value> = self
            .ledger
            .history(node, limit)?
            .into_iter()
            .map(|e| {
                json!({
                    "at_secs": e.at_secs,
                    "kind": e.kind.as_str(),
                    "amount_msat": e.amount_msat,
                    "fee_msat": e.fee_msat,
                    "payment_hash": e.payment_hash.map(hex::encode),
                })
            })
            .collect();
        Ok((200, Value::Array(entries)))
    }

    // ---- LDK event hooks (call before `event_handled`) ----

    /// A payment landed on `node`'s Fortis balance: tell the phone by push
    /// unless it is connected (its app shows the receipt then).
    fn notify_received(&self, node: &PublicKey, amount_msat: u64) {
        self.notices.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !self.node.is_connected(node) {
            crate::push::received(self.fcm.as_ref(), &self.ledger, node, amount_msat, self.chain);
        }
    }

    /// A payment arrived; credits the account its invoice belongs to. True
    /// when it was an account payment.
    pub fn on_payment_received(
        &self,
        payment_hash: [u8; 32],
        amount_msat: u64,
    ) -> Result<bool, LedgerError> {
        match self.ledger.invoice_owner(payment_hash)? {
            Some(node) => {
                // LDK hands old PaymentReceived events out again after a restart: the
                // ledger credits a payment once, and only a new credit is announced.
                if self.ledger.credit(&node, payment_hash, amount_msat)? {
                    self.notify_received(&node, amount_msat);
                }
                Ok(true)
            }
            // Not an account's. A transfer fee is the operator's revenue (recorded here).
            None => {
                self.ledger.fee_invoice_paid(payment_hash, amount_msat)?;
                Ok(false)
            }
        }
    }

    /// An inbound payment that was not an account invoice: if it paid an
    /// account's reusable offer, credit that account.
    pub fn on_offer_payment_received(
        &self,
        payment_id: [u8; 32],
        payment_hash: [u8; 32],
        amount_msat: u64,
    ) -> Result<bool, LedgerError> {
        let Some(offer_id) = self.node.paid_offer(payment_id) else {
            return Ok(false);
        };
        match self.ledger.offer_owner(offer_id)? {
            Some(node) => {
                if self.ledger.credit(&node, payment_hash, amount_msat)? {
                    eprintln!("fortis-lsp: offer payment of {amount_msat} msat to {node}");
                    self.notify_received(&node, amount_msat);
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// An outgoing payment resolved; settles an account send's hold.
    pub fn on_payment_resolved(
        &self,
        payment_hash: [u8; 32],
        fee_msat: Option<u64>,
    ) -> Result<(), LedgerError> {
        let id = send_hold_id(&payment_hash);
        if self.ledger.hold_owner(&id)?.is_none() {
            return Ok(());
        }
        match fee_msat {
            Some(fee) => self.ledger.settle_send(&id, fee, Some(payment_hash)),
            None => self.ledger.settle_hold(&id, 0, 0, None),
        }
    }
}

/// `AccountNode` over the real node.
pub struct LdkAccountNode(pub Arc<ldk_node::Node>);

impl AccountNode for LdkAccountNode {
    fn is_connected(&self, node: &PublicKey) -> bool {
        self.0.list_peers().iter().any(|p| p.node_id == *node && p.is_connected)
    }
    fn receive_invoice(
        &self,
        amount_msat: u64,
        description: &str,
        expiry_secs: u32,
    ) -> Result<(String, [u8; 32]), String> {
        use ldk_node::bitcoin::hashes::Hash;
        use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
        let desc = Description::new(description.to_string()).map_err(|e| format!("{e:?}"))?;
        let inv = self
            .0
            .bolt11_payment()
            .receive(
                amount_msat,
                &Bolt11InvoiceDescription::Direct(desc),
                expiry_secs,
            )
            .map_err(|e| e.to_string())?;
        Ok((inv.to_string(), inv.payment_hash().to_byte_array()))
    }

    fn invoice_details(&self, invoice: &str) -> Result<(Option<u64>, [u8; 32]), String> {
        use ldk_node::bitcoin::hashes::Hash;
        let inv = invoice
            .trim()
            .parse::<ldk_node::lightning_invoice::Bolt11Invoice>()
            .map_err(|e| e.to_string())?;
        Ok((
            inv.amount_milli_satoshis(),
            inv.payment_hash().to_byte_array(),
        ))
    }

    fn send_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<(), String> {
        let inv = invoice
            .trim()
            .parse::<ldk_node::lightning_invoice::Bolt11Invoice>()
            .map_err(|e| e.to_string())?;
        let p = self.0.bolt11_payment();
        match amount_msat {
            Some(a) => p.send_using_amount(&inv, a, None),
            None => p.send(&inv, None),
        }
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    fn send_onchain(&self, address: &str, amount_sat: u64, feerate_sat_vb: u64) -> Result<[u8; 32], Option<String>> {
        use ldk_node::bitcoin::hashes::Hash;
        use ldk_node::bitcoin::{address::NetworkUnchecked, Address, FeeRate};
        let addr = address
            .trim()
            .parse::<Address<NetworkUnchecked>>()
            .ok()
            .and_then(|a| a.require_network(self.0.config().network).ok())
            .ok_or(None)?;
        let rate = FeeRate::from_sat_per_vb(feerate_sat_vb).ok_or(None)?;
        // The wallet must know what is already spent (e.g. a funding just
        // broadcast), or the send could pick a spent coin and be rejected.
        self.0.sync_wallets().map_err(|e| Some(format!("sync: {e}")))?;
        self.0
            .onchain_payment()
            .send_to_address(&addr, amount_sat, Some(rate))
            .map(|t| t.to_byte_array())
            .map_err(|e| Some(e.to_string()))
    }

    fn receive_for_hash(&self, amount_msat: u64, description: &str, expiry_secs: u32, payment_hash: [u8; 32]) -> Result<String, String> {
        use ldk_node::lightning_invoice::{Bolt11InvoiceDescription, Description};
        let desc = Description::new(description.to_string()).map_err(|e| format!("{e:?}"))?;
        self.0
            .bolt11_payment()
            .receive_for_hash(
                amount_msat,
                &Bolt11InvoiceDescription::Direct(desc),
                expiry_secs,
                ldk_node::lightning::types::payment::PaymentHash(payment_hash),
            )
            .map(|i| i.to_string())
            .map_err(|e| e.to_string())
    }

    fn create_offer(&self, description: &str) -> Result<(String, [u8; 32]), String> {
        let offer = self
            .0
            .bolt12_payment()
            .receive_variable_amount(description, None)
            .map_err(|e| e.to_string())?;
        Ok((offer.to_string(), offer.id().0))
    }

    fn paid_offer(&self, payment_id: [u8; 32]) -> Option<[u8; 32]> {
        let p = self.0.payment(&ldk_node::lightning::ln::channelmanager::PaymentId(payment_id))?;
        match p.kind {
            ldk_node::payment::PaymentKind::Bolt12Offer { offer_id, .. } => Some(offer_id.0),
            _ => None,
        }
    }

    fn backing_sat(&self) -> u64 {
        let outbound: u64 = self
            .0
            .list_channels()
            .iter()
            .map(|c| c.outbound_capacity_msat / 1_000)
            .sum();
        self.0.list_balances().spendable_onchain_balance_sats + outbound
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_auth::tests::{body, key, signed};
    use std::sync::Mutex;

    type FakeInvoice = (String, Option<u64>, [u8; 32]);

    /// A node that "issues" invoices `inv-<n>` with hash [n; 32] and records sends.
    #[derive(Default)]
    struct FakeNode {
        next: Mutex<u8>,
        sent: Mutex<Vec<(String, Option<u64>)>>,
        fail_send: bool,
        backing: u64,
        invoices: Mutex<Vec<FakeInvoice>>,
    }

    impl FakeNode {
        fn with_backing(sat: u64) -> FakeNode {
            FakeNode {
                backing: sat,
                ..Default::default()
            }
        }
        fn foreign(&self, inv: &str, amount: Option<u64>, hash: [u8; 32]) {
            self.invoices
                .lock()
                .unwrap()
                .push((inv.into(), amount, hash));
        }
    }

    impl AccountNode for FakeNode {
        fn receive_invoice(
            &self,
            amount_msat: u64,
            _d: &str,
            _e: u32,
        ) -> Result<(String, [u8; 32]), String> {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            let inv = format!("inv-{n}");
            self.invoices
                .lock()
                .unwrap()
                .push((inv.clone(), Some(amount_msat), [*n; 32]));
            Ok((inv, [*n; 32]))
        }
        fn invoice_details(&self, invoice: &str) -> Result<(Option<u64>, [u8; 32]), String> {
            self.invoices
                .lock()
                .unwrap()
                .iter()
                .find(|(i, _, _)| i == invoice)
                .map(|(_, a, h)| (*a, *h))
                .ok_or_else(|| "unknown invoice".into())
        }
        fn send_bolt11(&self, invoice: &str, amount_msat: Option<u64>) -> Result<(), String> {
            if self.fail_send {
                return Err("no route".into());
            }
            self.sent
                .lock()
                .unwrap()
                .push((invoice.into(), amount_msat));
            Ok(())
        }
        fn backing_sat(&self) -> u64 {
            self.backing
        }
        fn receive_for_hash(&self, amount_msat: u64, _d: &str, _e: u32, hash: [u8; 32]) -> Result<String, String> {
            self.invoices.lock().unwrap().push(("held".into(), Some(amount_msat), hash));
            Ok("lnbcheld".into())
        }
        fn create_offer(&self, _d: &str) -> Result<(String, [u8; 32]), String> {
            Ok(("lno1fake".into(), [9; 32]))
        }
        fn paid_offer(&self, payment_id: [u8; 32]) -> Option<[u8; 32]> {
            (payment_id == [5; 32]).then_some([9; 32])
        }
        fn send_onchain(&self, address: &str, amount_sat: u64, feerate: u64) -> Result<[u8; 32], Option<String>> {
            if !address.starts_with("bc1") {
                return Err(None);
            }
            if self.fail_send {
                return Err(Some("insufficient funds".into()));
            }
            self.sent.lock().unwrap().push((format!("{address}@{feerate}"), Some(amount_sat)));
            Ok([7; 32])
        }
    }

    const NOW: u64 = 1_800_000_000;

    fn service(tag: &str, node: FakeNode, custody_max_sat: u64) -> (AccountService, Arc<FakeNode>) {
        let dir = std::env::temp_dir().join(format!("fortis-lsp-api-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ledger = Arc::new(Ledger::open(&dir.join("a.sqlite")).unwrap());
        let node = Arc::new(node);
        (
            AccountService::new(ledger, node.clone(), 50_000, custody_max_sat, 11_000, crate::config::Chain::Btc),
            node,
        )
    }

    /// Signs and sends one request as key `k`.
    fn call(svc: &AccountService, k: u8, op: &str, params: &str, nonce: &str) -> Reply {
        let (sk, pk) = key(k);
        let b = body(&pk, op, params, nonce, NOW + 60);
        svc.handle(b.as_bytes(), Some(&signed(&sk, &b)), NOW)
    }

    fn nonce(i: u32) -> String {
        format!("nonce-{i:010}-abcdef")
    }

    #[test]
    fn balance_of_a_new_account_is_zero() {
        let (svc, _) = service("bal", FakeNode::with_backing(1_000_000), 1_000_000);
        let (s, v) = call(&svc, 1, "balance", "{}", &nonce(1));
        assert_eq!(s, 200);
        assert_eq!(
            v,
            json!({ "balance_msat": 0, "held_msat": 0, "max_msat": 50_000_000 })
        );
    }

    #[test]
    fn a_fee_invoice_is_issued_even_with_accounts_off_and_credits_no_one() {
        let (svc, node) = service("fee", FakeNode::with_backing(0), 0);
        let (s, v) = call(&svc, 1, "fee_invoice", r#"{"amount_msat":1500000}"#, &nonce(1));
        assert_eq!((s, v["invoice"].as_str()), (200, Some("inv-1")), "{v}");
        let hash = node.invoices.lock().unwrap()[0].2;
        assert!(!svc.on_payment_received(hash, 1_500_000).unwrap(), "operator revenue");
        let rev = svc.ledger.revenue_since(0).unwrap();
        assert_eq!((rev.len(), rev[0].kind.as_str(), rev[0].amount_msat), (1, "transfer_fee", 1_500_000));
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(2));
        assert_eq!(b["balance_msat"], 0);
        for bad in [r#"{"amount_msat":0}"#, r#"{"amount_msat":50000001}"#, "{}"] {
            assert_eq!(call(&svc, 1, "fee_invoice", bad, &nonce(3 + bad.len() as u32)).0, 400, "{bad}");
        }
    }

    #[test]
    fn withdrawal_fee_follows_the_transfer_rule() {
        // 1 % wins; network 400 sat, operator keeps 600.
        assert_eq!(withdrawal(100_000, 2), Some(Withdrawal { payout_sat: 99_000, fee_sat: 1_000 }));
        // The network fee wins: the operator keeps nothing.
        assert_eq!(withdrawal(100_000, 10), Some(Withdrawal { payout_sat: 98_000, fee_sat: 2_000 }));
        // A share below dust is dropped: only the network fee.
        assert_eq!(withdrawal(50_000, 3), Some(Withdrawal { payout_sat: 49_400, fee_sat: 600 }));
        assert_eq!(withdrawal(1_500, 2), None, "too little would arrive");
    }

    #[test]
    fn withdraw_debits_the_account_and_sends_the_rest_on_chain() {
        let (svc, node) = service("wd", FakeNode::with_backing(10_000_000), 10_000_000);
        assert!(svc.on_payment_received([200; 32], 0).unwrap() == false);
        let (_, v) = call(&svc, 1, "invoice", r#"{"amount_msat":60000000}"#, &nonce(1));
        let hash = node.invoices.lock().unwrap()[0].2;
        assert!(svc.on_payment_received(hash, 60_000_000).unwrap(), "{v}");
        let p = r#"{"amount_msat":50000000,"address":"bc1qexample","feerate_sat_vb":2}"#;
        let (s, v) = call(&svc, 1, "withdraw", p, &nonce(2));
        assert_eq!((s, v["payout_sat"].as_u64(), v["fee_sat"].as_u64()), (200, Some(49_000), Some(1_000)), "{v}");
        assert_eq!(node.sent.lock().unwrap().last().unwrap(), &("bc1qexample@2".to_string(), Some(49_000)));
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(3));
        assert_eq!((b["balance_msat"].as_u64(), b["held_msat"].as_u64()), (Some(10_000_000), Some(0)));
        let (_, h) = call(&svc, 1, "history", r#"{"limit":1}"#, &nonce(4));
        assert_eq!((h[0]["kind"].as_str(), h[0]["fee_msat"].as_u64()), (Some("withdrawn"), Some(1_000_000)), "{h}");
        // More than is left; a foreign address; a failed send releases the hold.
        assert_eq!(call(&svc, 1, "withdraw", p, &nonce(5)).0, 402);
        let bad = r#"{"amount_msat":5000000,"address":"tb1qwrong","feerate_sat_vb":2}"#;
        assert_eq!(call(&svc, 1, "withdraw", bad, &nonce(6)).0, 400);
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(7));
        assert_eq!(b["held_msat"].as_u64(), Some(0));
    }

    #[test]
    fn an_offer_is_made_once_and_its_payments_credit_the_account() {
        let (svc, _) = service("offer", FakeNode::with_backing(1_000_000), 1_000_000);
        let (s, v) = call(&svc, 1, "offer", "{}", &nonce(1));
        assert_eq!((s, v["offer"].as_str()), (200, Some("lno1fake")));
        let (_, v) = call(&svc, 1, "offer", "{}", &nonce(2));
        assert_eq!(v["offer"].as_str(), Some("lno1fake"), "the same offer again");
        // A payment to it (payment id [5; 32] paid offer [9; 32]) credits key 1's account.
        assert!(svc.on_offer_payment_received([5; 32], [6; 32], 21_000).unwrap());
        assert!(!svc.on_offer_payment_received([7; 32], [8; 32], 21_000).unwrap(), "not an offer payment");
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(3));
        assert_eq!(b["balance_msat"].as_u64(), Some(21_000));
        // Accounts off: no offer.
        let (off, _) = service("offer-off", FakeNode::with_backing(1_000_000), 0);
        assert_eq!(call(&off, 1, "offer", "{}", &nonce(4)).0, 409);
    }

    #[test]
    fn a_replayed_receipt_is_credited_and_announced_once() {
        let (svc, node) = service("replay", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":5000}"#, &nonce(1));
        let hash = node.invoices.lock().unwrap()[0].2;
        assert!(svc.on_payment_received(hash, 5_000).unwrap());
        // The same event again after a restart: still recognised, not credited twice.
        assert!(svc.on_payment_received(hash, 5_000).unwrap());
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(2));
        assert_eq!(b["balance_msat"].as_u64(), Some(5_000));
        assert_eq!(svc.notices_sent(), 1, "announced once");
    }

    #[test]
    fn a_push_token_is_registered_replaced_and_dropped() {
        let (svc, _) = service("push", FakeNode::with_backing(1_000_000), 0);
        let (_, pk) = key(1);
        assert_eq!(call(&svc, 1, "push_register", r#"{"token":"tok-a"}"#, &nonce(1)).0, 200);
        assert_eq!(svc.ledger.push_token(&pk).unwrap().as_deref(), Some("tok-a"));
        // A new token (the app reinstalled, or Firebase rotated it) replaces the old one.
        assert_eq!(call(&svc, 1, "push_register", r#"{"token":"tok-b"}"#, &nonce(2)).0, 200);
        assert_eq!(svc.ledger.push_token(&pk).unwrap().as_deref(), Some("tok-b"));
        // Dropping a stale token leaves a newer one alone.
        svc.ledger.drop_push_token(&pk, "tok-a").unwrap();
        assert_eq!(svc.ledger.push_token(&pk).unwrap().as_deref(), Some("tok-b"));
        svc.ledger.drop_push_token(&pk, "tok-b").unwrap();
        assert_eq!(svc.ledger.push_token(&pk).unwrap(), None);
        // Not a token: empty, spaces, too long, missing.
        for (i, bad) in [r#"{"token":""}"#, r#"{"token":"a b"}"#, r#"{}"#].into_iter().enumerate() {
            assert_eq!(call(&svc, 1, "push_register", bad, &nonce(10 + i as u32)).0, 400, "{bad}");
        }
        let long = format!(r#"{{"token":"{}"}}"#, "x".repeat(MAX_PUSH_TOKEN_LEN + 1));
        assert_eq!(call(&svc, 1, "push_register", &long, &nonce(20)).0, 400);
    }

    #[test]
    fn account_invoice_refused_when_disabled() {
        let (svc, _) = service("off", FakeNode::with_backing(1_000_000), 0);
        let (s, v) = call(&svc, 1, "invoice", r#"{"amount_msat":100000}"#, &nonce(1));
        assert_eq!(
            (s, v["error"].as_str(), v["min_channel_sat"].as_u64()),
            (409, Some("custody_full"), Some(11_000))
        );
    }

    #[test]
    fn account_invoice_refused_at_total_custody() {
        let (svc, _) = service("full", FakeNode::with_backing(10_000_000), 1_000);
        svc.on_payment_received([200; 32], 0).unwrap();
        let (s, _) = call(&svc, 1, "invoice", r#"{"amount_msat":900000}"#, &nonce(1));
        assert_eq!(s, 200, "within the 1,000 sat custody");
        // Credit it, then another 200 sat would pass 1,000 sat.
        assert!(svc.on_payment_received([1; 32], 900_000).unwrap());
        let (s, v) = call(&svc, 1, "invoice", r#"{"amount_msat":200000}"#, &nonce(2));
        assert_eq!((s, v["error"].as_str()), (409, Some("custody_full")));
    }

    #[test]
    fn account_invoice_refused_above_backing() {
        let (svc, _) = service("backing", FakeNode::with_backing(50), 1_000_000);
        let (s, _) = call(&svc, 1, "invoice", r#"{"amount_msat":100000}"#, &nonce(1));
        assert_eq!(s, 409, "the node could only pay out 50 sat");
    }

    #[test]
    fn an_account_invoice_credits_its_account_once() {
        let (svc, _) = service("credit", FakeNode::with_backing(1_000_000), 1_000_000);
        let (s, v) = call(
            &svc,
            1,
            "invoice",
            r#"{"amount_msat":100000,"description":"tip"}"#,
            &nonce(1),
        );
        assert_eq!((s, v["invoice"].as_str()), (200, Some("inv-1")));
        assert!(svc.on_payment_received([1; 32], 100_000).unwrap());
        // The same event replayed after a restart:
        assert!(svc.on_payment_received([1; 32], 100_000).unwrap());
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(2));
        assert_eq!(b["balance_msat"], 100_000);
        // A payment for an invoice that isn't an account's is not ours to credit.
        assert!(!svc.on_payment_received([99; 32], 5_000).unwrap());
    }

    #[test]
    fn pay_holds_amount_plus_fee_cap_and_debits_the_actual_fee() {
        let (svc, node) = service("pay", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":200000}"#, &nonce(1));
        svc.on_payment_received([1; 32], 200_000).unwrap();
        node.foreign("lnbc-out", Some(30_000), [50; 32]);
        let (s, v) = call(&svc, 1, "pay", r#"{"invoice":"lnbc-out"}"#, &nonce(2));
        assert_eq!((s, v["internal"].as_bool()), (200, Some(false)));
        assert_eq!(
            node.sent.lock().unwrap().as_slice(),
            &[("lnbc-out".to_string(), None)]
        );
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(3));
        assert_eq!(b["held_msat"], 30_000 + 300 + 50_000);
        svc.on_payment_resolved([50; 32], Some(1_000)).unwrap();
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(4));
        assert_eq!(
            (b["balance_msat"].as_u64(), b["held_msat"].as_u64()),
            (Some(200_000 - 31_000), Some(0))
        );
    }

    #[test]
    fn failed_send_releases_the_hold() {
        let (svc, node) = service("fail", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":200000}"#, &nonce(1));
        svc.on_payment_received([1; 32], 200_000).unwrap();
        node.foreign("lnbc-out", Some(30_000), [50; 32]);
        call(&svc, 1, "pay", r#"{"invoice":"lnbc-out"}"#, &nonce(2));
        svc.on_payment_resolved([50; 32], None).unwrap();
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(3));
        assert_eq!(
            (b["balance_msat"].as_u64(), b["held_msat"].as_u64()),
            (Some(200_000), Some(0))
        );
        // A send the node refuses at once also gives the hold back.
        let (svc, node) = service(
            "fail2",
            FakeNode {
                fail_send: true,
                backing: 1_000_000,
                ..Default::default()
            },
            1_000_000,
        );
        call(&svc, 1, "invoice", r#"{"amount_msat":200000}"#, &nonce(1));
        svc.on_payment_received([1; 32], 200_000).unwrap();
        node.foreign("lnbc-out", Some(30_000), [50; 32]);
        let (s, _) = call(&svc, 1, "pay", r#"{"invoice":"lnbc-out"}"#, &nonce(2));
        assert_eq!(s, 502);
        let (_, b) = call(&svc, 1, "balance", "{}", &nonce(3));
        assert_eq!(b["held_msat"], 0);
    }

    #[test]
    fn paying_without_enough_is_402_and_sends_nothing() {
        let (svc, node) = service("poor", FakeNode::with_backing(1_000_000), 1_000_000);
        node.foreign("lnbc-out", Some(30_000), [50; 32]);
        let (s, v) = call(&svc, 1, "pay", r#"{"invoice":"lnbc-out"}"#, &nonce(1));
        assert_eq!((s, v["error"].as_str()), (402, Some("insufficient")));
        assert!(node.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn paying_another_accounts_invoice_is_internal_and_free() {
        let (svc, node) = service("internal", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":100000}"#, &nonce(1)); // inv-1 for key 1
        svc.on_payment_received([1; 32], 100_000).unwrap();
        let (_, v) = call(&svc, 2, "invoice", r#"{"amount_msat":40000}"#, &nonce(2)); // inv-2 for key 2
        let inv2 = v["invoice"].as_str().unwrap().to_string();
        let (s, v) = call(
            &svc,
            1,
            "pay",
            &format!(r#"{{"invoice":"{inv2}"}}"#),
            &nonce(3),
        );
        assert_eq!(
            (s, v["internal"].as_bool(), v["fee_msat"].as_u64()),
            (200, Some(true), Some(0))
        );
        assert!(node.sent.lock().unwrap().is_empty(), "no Lightning payment");
        let (_, a) = call(&svc, 1, "balance", "{}", &nonce(4));
        let (_, b) = call(&svc, 2, "balance", "{}", &nonce(5));
        assert_eq!(
            (a["balance_msat"].as_u64(), b["balance_msat"].as_u64()),
            (Some(60_000), Some(40_000))
        );
        // Paying your own account invoice is refused.
        let (_, v) = call(&svc, 2, "invoice", r#"{"amount_msat":1000}"#, &nonce(6));
        let own = v["invoice"].as_str().unwrap().to_string();
        let (s, v) = call(
            &svc,
            2,
            "pay",
            &format!(r#"{{"invoice":"{own}"}}"#),
            &nonce(7),
        );
        assert_eq!((s, v["error"].as_str()), (400, Some("own_invoice")));
    }

    #[test]
    fn amountless_invoices_need_an_amount_and_amounts_must_match() {
        let (svc, node) = service("amounts", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":200000}"#, &nonce(1));
        svc.on_payment_received([1; 32], 200_000).unwrap();
        node.foreign("lnbc-any", None, [60; 32]);
        node.foreign("lnbc-fixed", Some(10_000), [61; 32]);
        assert_eq!(
            call(&svc, 1, "pay", r#"{"invoice":"lnbc-any"}"#, &nonce(2)).1["error"],
            "amount_required"
        );
        assert_eq!(
            call(
                &svc,
                1,
                "pay",
                r#"{"invoice":"lnbc-fixed","amount_msat":9000}"#,
                &nonce(3)
            )
            .1["error"],
            "amount_mismatch"
        );
        assert_eq!(
            call(
                &svc,
                1,
                "pay",
                r#"{"invoice":"lnbc-any","amount_msat":5000}"#,
                &nonce(4)
            )
            .0,
            200
        );
        assert_eq!(
            node.sent.lock().unwrap().last().unwrap(),
            &("lnbc-any".to_string(), Some(5_000))
        );
    }

    #[test]
    fn bad_signature_is_401_and_changes_nothing() {
        let (svc, _) = service("sig", FakeNode::with_backing(1_000_000), 1_000_000);
        let (_, pk) = key(1);
        let (other, _) = key(2);
        let b = body(
            &pk,
            "invoice",
            r#"{"amount_msat":1000}"#,
            &nonce(1),
            NOW + 60,
        );
        let (s, v) = svc.handle(b.as_bytes(), Some(&signed(&other, &b)), NOW);
        assert_eq!((s, v["reason"].as_str()), (401, Some("bad_signature")));
        let (s, _) = svc.handle(b.as_bytes(), None, NOW);
        assert_eq!(s, 401);
        assert_eq!(svc.custody_sat(), 0);
    }

    #[test]
    fn history_lists_entries_newest_first() {
        let (svc, _) = service("hist", FakeNode::with_backing(1_000_000), 1_000_000);
        call(&svc, 1, "invoice", r#"{"amount_msat":1000}"#, &nonce(1));
        call(&svc, 1, "invoice", r#"{"amount_msat":2000}"#, &nonce(2));
        svc.on_payment_received([1; 32], 1_000).unwrap();
        svc.on_payment_received([2; 32], 2_000).unwrap();
        let (s, v) = call(&svc, 1, "history", r#"{"limit":10}"#, &nonce(3));
        assert_eq!(s, 200);
        let a = v.as_array().unwrap();
        assert_eq!(
            (a[0]["amount_msat"].as_i64(), a[0]["kind"].as_str()),
            (Some(2_000), Some("received"))
        );
        assert_eq!(a.len(), 2);
    }
}
