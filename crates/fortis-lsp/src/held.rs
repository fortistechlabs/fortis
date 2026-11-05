//! Payments into a phone's own channel, held by the LSP until the phone can
//! take them, so the phone need not be running when it is paid.
//!
//! The phone asks for an invoice (`channel_invoice`); the LSP issues it and
//! keeps the preimage. A payment to it reaches the LSP and waits there,
//! unsettled (`PaymentClaimable`). As soon as the phone is connected and its
//! channel has room, the LSP pays it the amount (keysend) and, once that
//! succeeded, settles the payer's payment. If the phone has not connected
//! [`SAFETY_BLOCKS`] before the payment would time out, the LSP settles it and
//! credits the phone's Fortis account instead (custodial; refused back to the
//! payer when the custody limit is reached).

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use ldk_node::bitcoin::hashes::{sha256, Hash};
use ldk_node::lightning::ln::channelmanager::PaymentId;
use ldk_node::lightning::types::payment::{PaymentHash, PaymentPreimage};

use crate::account::{Held, LedgerError};
use crate::account_api::AccountService;

/// Settle into the account this many blocks before the payment times out.
pub const SAFETY_BLOCKS: u32 = 6;
/// How often held payments are looked at: a phone that just connected gets
/// its payment within about a second.
pub const CHECK: Duration = Duration::from_secs(1);

/// Phones already asked to wake for a held payment (one push per payment).
static WOKEN: Mutex<Option<HashSet<[u8; 32]>>> = Mutex::new(None);

pub fn new_preimage() -> [u8; 32] {
    let mut b = [0u8; 32];
    let _ = std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").expect("/dev/urandom"),
        &mut b,
    );
    b
}

pub fn hash_of(preimage: &[u8; 32]) -> [u8; 32] {
    sha256::Hash::hash(preimage).to_byte_array()
}

/// A payment became claimable. `Ok(false)`: not a held invoice.
pub fn on_claimable(
    node: &ldk_node::Node,
    accounts: &AccountService,
    hash: [u8; 32],
    amount_msat: u64,
    deadline: Option<u32>,
) -> Result<bool, LedgerError> {
    let ledger = &accounts.ledger;
    let Some(h) = ledger.held_get(hash)? else {
        return Ok(false);
    };
    if h.state == "open" {
        ledger.held_update(hash, "claimable", Some((amount_msat, deadline)), None)?;
        eprintln!("fortis-lsp: holding {amount_msat} msat for {}", h.node);
        if let Some(h) = ledger.held_get(hash)? {
            deliver(node, accounts, &h)?;
        }
    }
    Ok(true)
}

/// Looks at every held payment: delivers it if the phone is there, settles
/// it into the account when its time is nearly up.
pub fn tick(node: &ldk_node::Node, accounts: &AccountService) {
    let held = match accounts.ledger.held_claimable() {
        Ok(h) => h,
        Err(e) => return eprintln!("fortis-lsp: held payments: {e}"),
    };
    for h in held {
        if let Err(e) = deliver(node, accounts, &h) {
            eprintln!("fortis-lsp: held payment for {}: {e}", h.node);
        }
    }
}

fn deliver(node: &ldk_node::Node, accounts: &AccountService, h: &Held) -> Result<(), LedgerError> {
    let Some(amount) = h.claimable_msat else { return Ok(()) };
    let height = node.status().current_best_block.height;
    let deadline = h.deadline.unwrap_or(u32::MAX);
    if h.state == "claimable" && height.saturating_add(SAFETY_BLOCKS) >= deadline {
        return settle_into_account(node, accounts, h, amount);
    }
    if h.state != "claimable" {
        return Ok(());
    }
    let connected = node.list_peers().iter().any(|p| p.node_id == h.node && p.is_connected);
    let room = node
        .list_channels()
        .iter()
        .filter(|c| c.counterparty_node_id == h.node && c.is_usable)
        .map(|c| c.next_outbound_htlc_limit_msat)
        .max()
        .unwrap_or(0);
    // Never forward so late that the payer's side could time out first.
    let time_left = height.saturating_add(2 * SAFETY_BLOCKS) < deadline;
    if connected && room >= amount && time_left {
        match node.spontaneous_payment().send(amount, h.node, None) {
            Ok(id) => {
                accounts.ledger.held_update(h.payment_hash, "forwarding", None, Some(Some(id.0)))?;
                eprintln!("fortis-lsp: delivering held {amount} msat to {}", h.node);
            }
            Err(e) => eprintln!("fortis-lsp: delivering held payment to {}: {e}", h.node),
        }
    } else if !connected {
        let mut woken = WOKEN.lock().unwrap_or_else(|e| e.into_inner());
        if woken.get_or_insert_with(HashSet::new).insert(h.payment_hash) {
            crate::push::wake(accounts.fcm.as_ref(), &accounts.ledger, &h.node, amount);
        }
    }
    Ok(())
}

fn settle_into_account(
    node: &ldk_node::Node,
    accounts: &AccountService,
    h: &Held,
    amount: u64,
) -> Result<(), LedgerError> {
    let room = accounts
        .custody_max_msat
        .saturating_sub(accounts.ledger.total_custody_msat()?);
    let p = node.bolt11_payment();
    if accounts.custody_max_msat > 0 && room >= amount {
        if let Err(e) = p.claim_for_hash(PaymentHash(h.payment_hash), amount, PaymentPreimage(h.preimage)) {
            eprintln!("fortis-lsp: settling held payment for {}: {e}", h.node);
            return Ok(());
        }
        accounts.ledger.credit(&h.node, h.payment_hash, amount)?;
        accounts.ledger.held_update(h.payment_hash, "credited", None, None)?;
        eprintln!("fortis-lsp: {} did not connect in time: {amount} msat kept on its Fortis balance", h.node);
    } else {
        let _ = p.fail_for_hash(PaymentHash(h.payment_hash));
        accounts.ledger.held_update(h.payment_hash, "failed", None, None)?;
        eprintln!("fortis-lsp: {} did not connect in time and custody is full: payment returned", h.node);
    }
    Ok(())
}

/// The keysend delivering a held payment succeeded or failed.
pub fn on_forward_result(
    node: &ldk_node::Node,
    accounts: &AccountService,
    payment_id: PaymentId,
    succeeded: bool,
) -> Result<(), LedgerError> {
    let Some(h) = accounts.ledger.held_by_forward(payment_id.0)? else {
        return Ok(());
    };
    if !succeeded {
        accounts.ledger.held_update(h.payment_hash, "claimable", None, Some(None))?;
        eprintln!("fortis-lsp: delivering held payment to {} failed; will retry", h.node);
        return Ok(());
    }
    let amount = h.claimable_msat.unwrap_or(h.amount_msat);
    match node
        .bolt11_payment()
        .claim_for_hash(PaymentHash(h.payment_hash), amount, PaymentPreimage(h.preimage))
    {
        Ok(()) => {
            accounts.ledger.held_update(h.payment_hash, "done", None, None)?;
            eprintln!("fortis-lsp: held payment delivered to {} and settled", h.node);
        }
        Err(e) => eprintln!("fortis-lsp: settling delivered payment for {}: {e}", h.node),
    }
    Ok(())
}

/// Moves every Fortis balance whose phone is connected and has a channel with
/// room into that channel (keysend, free): an offer payment, or a held one
/// settled into the account, reaches the user's own channel as soon as the
/// phone is back. The hold is `deliver:pay:<payment id>`, debited when the
/// keysend succeeds (`on_delivery_result`).
pub fn deliver_balances(node: &ldk_node::Node, accounts: &AccountService) {
    let ledger = &accounts.ledger;
    let Ok(with_balance) = ledger.over_cap(0) else { return };
    if with_balance.is_empty() {
        return;
    }
    let busy: HashSet<_> = ledger.conversions().unwrap_or_default().into_iter().map(|(_, n)| n).collect();
    let peers = node.list_peers();
    let channels = node.list_channels();
    for (pk, acct) in with_balance {
        let free = acct.free_msat();
        if free < 1_000 || busy.contains(&pk) {
            continue;
        }
        if !peers.iter().any(|p| p.node_id == pk && p.is_connected) {
            continue;
        }
        let room = channels
            .iter()
            .filter(|c| c.counterparty_node_id == pk && c.is_usable)
            .map(|c| c.next_outbound_htlc_limit_msat)
            .max()
            .unwrap_or(0);
        if room < free {
            continue; // no channel, or too small: the cap conversion handles it
        }
        let tmp = format!("deliver:pending:{pk}");
        let Ok(amount) = ledger.start_conversion(&pk, &tmp) else { continue };
        match node.spontaneous_payment().send(amount, pk, None) {
            Ok(id) => {
                let _ = ledger.rename_hold(&tmp, &format!("deliver:pay:{}", hex::encode(id.0)));
                eprintln!("fortis-lsp: moving {amount} msat from {pk}'s Fortis balance into its channel");
            }
            Err(e) => {
                let _ = ledger.abandon_conversion(&tmp);
                eprintln!("fortis-lsp: moving {pk}'s balance into its channel: {e}");
            }
        }
    }
}

/// A balance delivery (`deliver_balances`) succeeded or failed.
pub fn on_delivery_result(accounts: &AccountService, payment_id: PaymentId, succeeded: bool) -> Result<(), LedgerError> {
    let hold = format!("deliver:pay:{}", hex::encode(payment_id.0));
    if accounts.ledger.hold_owner(&hold)?.is_none() {
        return Ok(());
    }
    if succeeded {
        accounts.ledger.finish_conversion(&hold, 0)?;
        eprintln!("fortis-lsp: Fortis balance moved into the user's channel ({hold})");
    } else {
        accounts.ledger.abandon_conversion(&hold)?;
    }
    Ok(())
}
