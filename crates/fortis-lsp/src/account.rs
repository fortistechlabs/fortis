//! The Fortis account ledger: small balances `fortis-lsp` holds for users
//! (custodial, capped), keyed by the user's Lightning node id. See the spec,
//! §7. One SQLite file, `<data_dir>/accounts.sqlite`, owner-only; every
//! change is one transaction committed with `synchronous=FULL` before the
//! caller acknowledges what it records (an LDK event, an HTTP reply).
//!
//! Amounts are msat. A *hold* reserves part of a balance for a payment in
//! flight (an account send, a conversion); settling it debits what was spent
//! and releases the rest.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ldk_node::bitcoin::secp256k1::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};

pub const LEDGER_FILE: &str = "accounts.sqlite";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Account {
    pub balance_msat: u64,
    pub held_msat: u64,
}

impl Account {
    /// What can be spent now.
    pub fn free_msat(&self) -> u64 {
        self.balance_msat.saturating_sub(self.held_msat)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Received,
    Sent,
    Internal,
    Converted,
    /// Moved out to an on-chain address (`withdraw`); `payment_hash` holds the txid.
    Withdrawn,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Received => "received",
            EntryKind::Sent => "sent",
            EntryKind::Internal => "internal",
            EntryKind::Converted => "converted",
            EntryKind::Withdrawn => "withdrawn",
        }
    }

    fn parse(s: &str) -> EntryKind {
        match s {
            "sent" => EntryKind::Sent,
            "internal" => EntryKind::Internal,
            "converted" => EntryKind::Converted,
            "withdrawn" => EntryKind::Withdrawn,
            _ => EntryKind::Received,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at_secs: u64,
    pub kind: EntryKind,
    /// Signed: positive into the account, negative out of it.
    pub amount_msat: i64,
    pub fee_msat: u64,
    pub payment_hash: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("not enough in the account")]
    InsufficientBalance,
    #[error("unknown hold {0}")]
    UnknownHold(String),
    #[error("ledger: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("ledger file: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, LedgerError>;

pub struct Ledger {
    db: Mutex<Connection>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn key(node: &PublicKey) -> String {
    node.to_string()
}

fn as_i64(msat: u64) -> i64 {
    i64::try_from(msat).unwrap_or(i64::MAX)
}

impl Ledger {
    /// Opens (creating, mode 0600) the ledger at `path`.
    pub fn open(path: &Path) -> Result<Ledger> {
        let new = !path.exists();
        let conn = Connection::open(path)?;
        if new {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS accounts (
               node TEXT PRIMARY KEY,
               balance_msat INTEGER NOT NULL DEFAULT 0 CHECK (balance_msat >= 0)
             );
             CREATE TABLE IF NOT EXISTS holds (
               id TEXT PRIMARY KEY,
               node TEXT NOT NULL,
               amount_msat INTEGER NOT NULL CHECK (amount_msat >= 0),
               base_msat INTEGER NOT NULL DEFAULT 0,
               kind TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS entries (
               seq INTEGER PRIMARY KEY AUTOINCREMENT,
               node TEXT NOT NULL,
               at_secs INTEGER NOT NULL,
               kind TEXT NOT NULL,
               amount_msat INTEGER NOT NULL,
               fee_msat INTEGER NOT NULL DEFAULT 0,
               payment_hash BLOB
             );
             CREATE UNIQUE INDEX IF NOT EXISTS credited_once
               ON entries(node, payment_hash) WHERE payment_hash IS NOT NULL AND kind IN ('received', 'internal');
             CREATE TABLE IF NOT EXISTS invoices (
               payment_hash BLOB PRIMARY KEY,
               node TEXT NOT NULL,
               amount_msat INTEGER NOT NULL,
               expires_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS push_tokens (
               node TEXT PRIMARY KEY,
               token TEXT NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS offers (
               offer_id BLOB PRIMARY KEY,
               node TEXT NOT NULL UNIQUE,
               offer TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS held (
               payment_hash BLOB PRIMARY KEY,
               node TEXT NOT NULL,
               preimage BLOB NOT NULL,
               amount_msat INTEGER NOT NULL,
               claimable_msat INTEGER,
               deadline INTEGER,
               forward_id BLOB,
               state TEXT NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS nonces (
               node TEXT NOT NULL,
               nonce TEXT NOT NULL,
               expires_at INTEGER NOT NULL,
               PRIMARY KEY (node, nonce)
             );",
        )?;
        Ok(Ledger {
            db: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn balance(&self, node: &PublicKey) -> Result<Account> {
        let c = self.conn();
        account_of(&c, &key(node))
    }

    /// Credits a payment received for `node`. `false` when this payment hash
    /// was already credited to it (an LDK event replayed after a restart).
    pub fn credit(
        &self,
        node: &PublicKey,
        payment_hash: [u8; 32],
        amount_msat: u64,
    ) -> Result<bool> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let k = key(node);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO entries (node, at_secs, kind, amount_msat, payment_hash) VALUES (?1, ?2, 'received', ?3, ?4)",
            params![k, now_secs() as i64, as_i64(amount_msat), payment_hash.to_vec()],
        )?;
        if inserted == 1 {
            add_balance(&tx, &k, as_i64(amount_msat))?;
        }
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Reserves `amount_msat` of `node`'s balance under `id`.
    #[cfg(test)]
    pub fn hold(&self, node: &PublicKey, id: &str, amount_msat: u64) -> Result<()> {
        self.hold_kind(node, id, amount_msat, 0, "send")
    }

    /// Reserves `reserve_msat` for an account send of `amount_msat` (the
    /// reserve covers the routing fee cap); `settle_send` debits the amount
    /// plus the fee actually paid.
    pub fn hold_send(
        &self,
        node: &PublicKey,
        id: &str,
        amount_msat: u64,
        reserve_msat: u64,
    ) -> Result<()> {
        self.hold_kind(node, id, reserve_msat.max(amount_msat), amount_msat, "send")
    }

    /// The account send under hold `id` succeeded, paying `fee_msat`.
    pub fn settle_send(
        &self,
        id: &str,
        fee_msat: u64,
        payment_hash: Option<[u8; 32]>,
    ) -> Result<()> {
        let base: Option<i64> = {
            let c = self.conn();
            c.query_row(
                "SELECT base_msat FROM holds WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
        };
        let base = base
            .ok_or_else(|| LedgerError::UnknownHold(id.to_string()))?
            .max(0) as u64;
        self.settle_hold(id, base.saturating_add(fee_msat), fee_msat, payment_hash)
    }

    fn hold_kind(
        &self,
        node: &PublicKey,
        id: &str,
        amount_msat: u64,
        base_msat: u64,
        kind: &str,
    ) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let k = key(node);
        if account_of(&tx, &k)?.free_msat() < amount_msat {
            return Err(LedgerError::InsufficientBalance);
        }
        tx.execute(
            "INSERT INTO holds (id, node, amount_msat, base_msat, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, k, as_i64(amount_msat), as_i64(base_msat), kind],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The node a hold belongs to, if the hold is still open.
    pub fn hold_owner(&self, id: &str) -> Result<Option<(PublicKey, u64)>> {
        let c = self.conn();
        let row: Option<(String, i64)> = c
            .query_row(
                "SELECT node, amount_msat FROM holds WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(n, a)| n.parse().ok().map(|pk| (pk, a.max(0) as u64))))
    }

    /// Closes hold `id` of an account send: debits `spent_msat` (amount +
    /// fee; 0 = the send failed) and releases the rest.
    pub fn settle_hold(
        &self,
        id: &str,
        spent_msat: u64,
        fee_msat: u64,
        payment_hash: Option<[u8; 32]>,
    ) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let (k, held) = take_hold(&tx, id)?;
        let spent = spent_msat.min(held);
        if spent > 0 {
            add_balance(&tx, &k, -as_i64(spent))?;
            tx.execute(
                "INSERT INTO entries (node, at_secs, kind, amount_msat, fee_msat, payment_hash) VALUES (?1, ?2, 'sent', ?3, ?4, ?5)",
                params![k, now_secs() as i64, -as_i64(spent), as_i64(fee_msat), payment_hash.map(|h| h.to_vec())],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Reserves `amount_msat` for an on-chain withdrawal (hold `id`).
    pub fn hold_withdrawal(&self, node: &PublicKey, id: &str, amount_msat: u64) -> Result<()> {
        self.hold_kind(node, id, amount_msat, amount_msat, "withdraw")
    }

    /// The withdrawal under hold `id` was broadcast as `txid`: debits the
    /// whole hold, `fee_msat` of it the fee.
    pub fn settle_withdrawal(&self, id: &str, fee_msat: u64, txid: [u8; 32]) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let (k, held) = take_hold(&tx, id)?;
        add_balance(&tx, &k, -as_i64(held))?;
        tx.execute(
            "INSERT INTO entries (node, at_secs, kind, amount_msat, fee_msat, payment_hash) VALUES (?1, ?2, 'withdrawn', ?3, ?4, ?5)",
            params![k, now_secs() as i64, -as_i64(held), as_i64(fee_msat), txid.to_vec()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Moves `amount_msat` from `from` to `to` (an account paying another
    /// account's invoice): atomic, once per payment hash.
    pub fn transfer(
        &self,
        from: &PublicKey,
        to: &PublicKey,
        payment_hash: [u8; 32],
        amount_msat: u64,
    ) -> Result<bool> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let (f, t) = (key(from), key(to));
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO entries (node, at_secs, kind, amount_msat, payment_hash) VALUES (?1, ?2, 'internal', ?3, ?4)",
            params![t, now_secs() as i64, as_i64(amount_msat), payment_hash.to_vec()],
        )?;
        if inserted == 1 {
            if account_of(&tx, &f)?.free_msat() < amount_msat {
                return Err(LedgerError::InsufficientBalance);
            }
            add_balance(&tx, &f, -as_i64(amount_msat))?;
            add_balance(&tx, &t, as_i64(amount_msat))?;
            tx.execute(
                "INSERT INTO entries (node, at_secs, kind, amount_msat, payment_hash) VALUES (?1, ?2, 'internal', ?3, NULL)",
                params![f, now_secs() as i64, -as_i64(amount_msat)],
            )?;
        }
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Holds `node`'s whole free balance for a conversion into its channel.
    /// Returns the amount held.
    pub fn start_conversion(&self, node: &PublicKey, id: &str) -> Result<u64> {
        let free = self.balance(node)?.free_msat();
        self.hold_kind(node, id, free, free, "conversion")?;
        Ok(free)
    }

    /// The conversion `id` arrived in the user's channel: debit it all,
    /// recording the fee the LSP kept.
    pub fn finish_conversion(&self, id: &str, fee_msat: u64) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let (k, held) = take_hold(&tx, id)?;
        add_balance(&tx, &k, -as_i64(held))?;
        tx.execute(
            "INSERT INTO entries (node, at_secs, kind, amount_msat, fee_msat) VALUES (?1, ?2, 'converted', ?3, ?4)",
            params![k, now_secs() as i64, -as_i64(held), as_i64(fee_msat)],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The conversion `id` failed (channel closed before ready): the balance
    /// stays in the account.
    pub fn abandon_conversion(&self, id: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        take_hold(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Renames hold `old` to `new` (a conversion learns its payment or
    /// channel id only after the hold was taken).
    pub fn rename_hold(&self, old: &str, new: &str) -> Result<()> {
        let c = self.conn();
        if c.execute("UPDATE holds SET id = ?2 WHERE id = ?1", params![old, new])? == 0 {
            return Err(LedgerError::UnknownHold(old.to_string()));
        }
        Ok(())
    }

    /// Open holds of kind "conversion": `(id, node)`.
    pub fn conversions(&self) -> Result<Vec<(String, PublicKey)>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, node FROM holds WHERE kind = 'conversion'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (id, n) = r?;
            if let Ok(pk) = n.parse() {
                out.push((id, pk));
            }
        }
        Ok(out)
    }

    /// Accounts holding more than `cap_msat`.
    pub fn over_cap(&self, cap_msat: u64) -> Result<Vec<(PublicKey, Account)>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT node FROM accounts WHERE balance_msat > ?1")?;
        let nodes: Vec<String> = st
            .query_map(params![as_i64(cap_msat)], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::new();
        for n in nodes {
            if let Ok(pk) = n.parse::<PublicKey>() {
                out.push((pk, account_of(&c, &n)?));
            }
        }
        Ok(out)
    }

    pub fn history(&self, node: &PublicKey, limit: usize) -> Result<Vec<Entry>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT at_secs, kind, amount_msat, fee_msat, payment_hash FROM entries WHERE node = ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![key(node), limit as i64], |r| {
            let hash: Option<Vec<u8>> = r.get(4)?;
            Ok(Entry {
                at_secs: r.get::<_, i64>(0)?.max(0) as u64,
                kind: EntryKind::parse(&r.get::<_, String>(1)?),
                amount_msat: r.get(2)?,
                fee_msat: r.get::<_, i64>(3)?.max(0) as u64,
                payment_hash: hash.and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok()),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// All balances (holds are part of balances until settled).
    pub fn total_custody_msat(&self) -> Result<u64> {
        let c = self.conn();
        let total: i64 = c.query_row(
            "SELECT COALESCE(SUM(balance_msat), 0) FROM accounts",
            [],
            |r| r.get(0),
        )?;
        Ok(total.max(0) as u64)
    }

    /// Marks `nonce` used by `node` until `expires_at`. `false` when it was
    /// already used. Prunes nonces whose request has expired.
    pub fn use_nonce(&self, node: &PublicKey, nonce: &str, expires_at: u64) -> Result<bool> {
        let c = self.conn();
        c.execute(
            "DELETE FROM nonces WHERE expires_at < ?1",
            params![now_secs() as i64],
        )?;
        let n = c.execute(
            "INSERT OR IGNORE INTO nonces (node, nonce, expires_at) VALUES (?1, ?2, ?3)",
            params![key(node), nonce, expires_at as i64],
        )?;
        Ok(n == 1)
    }

    /// Records that the invoice with `payment_hash` pays into `node`'s account.
    pub fn pending_invoice(
        &self,
        payment_hash: [u8; 32],
        node: &PublicKey,
        amount_msat: u64,
        expires_at: u64,
    ) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT OR REPLACE INTO invoices (payment_hash, node, amount_msat, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![payment_hash.to_vec(), key(node), as_i64(amount_msat), expires_at as i64],
        )?;
        Ok(())
    }

    /// The account an invoice of ours pays into.
    /// The account's reusable BOLT12 offer, if it has one.
    pub fn offer_of(&self, node: &PublicKey) -> Result<Option<String>> {
        let c = self.conn();
        Ok(c.query_row("SELECT offer FROM offers WHERE node = ?1", params![key(node)], |r| r.get(0))
            .optional()?)
    }

    /// Records `node`'s reusable offer (one per account).
    /// The phone's push token (Firebase Cloud Messaging), to wake it for a held payment.
    pub fn set_push_token(&self, node: &PublicKey, token: &str, now: u64) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT OR REPLACE INTO push_tokens (node, token, updated_at) VALUES (?1, ?2, ?3)",
            params![key(node), token, now as i64],
        )?;
        Ok(())
    }

    pub fn push_token(&self, node: &PublicKey) -> Result<Option<String>> {
        let c = self.conn();
        let t = c
            .query_row(
                "SELECT token FROM push_tokens WHERE node = ?1",
                params![key(node)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(t)
    }

    /// Forgets a token Firebase reported as no longer valid.
    pub fn drop_push_token(&self, node: &PublicKey, token: &str) -> Result<()> {
        let c = self.conn();
        c.execute(
            "DELETE FROM push_tokens WHERE node = ?1 AND token = ?2",
            params![key(node), token],
        )?;
        Ok(())
    }

    pub fn set_offer(&self, node: &PublicKey, offer_id: [u8; 32], offer: &str) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT OR REPLACE INTO offers (offer_id, node, offer) VALUES (?1, ?2, ?3)",
            params![offer_id.to_vec(), key(node), offer],
        )?;
        Ok(())
    }

    /// The account an offer belongs to.
    pub fn offer_owner(&self, offer_id: [u8; 32]) -> Result<Option<PublicKey>> {
        let c = self.conn();
        let n: Option<String> = c
            .query_row("SELECT node FROM offers WHERE offer_id = ?1", params![offer_id.to_vec()], |r| r.get(0))
            .optional()?;
        Ok(n.and_then(|n| n.parse().ok()))
    }

    /// A held invoice for `node` (the LSP keeps the preimage; see `held.rs`).
    pub fn held_insert(&self, hash: [u8; 32], node: &PublicKey, preimage: [u8; 32], amount_msat: u64) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO held (payment_hash, node, preimage, amount_msat, state, created_at) VALUES (?1, ?2, ?3, ?4, 'open', ?5)",
            params![hash.to_vec(), key(node), preimage.to_vec(), as_i64(amount_msat), now_secs() as i64],
        )?;
        Ok(())
    }

    pub fn held_get(&self, hash: [u8; 32]) -> Result<Option<Held>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {HELD_COLS} FROM held WHERE payment_hash = ?1"),
            params![hash.to_vec()],
            held_row,
        )
        .optional()?)
    }

    /// Held payments the payer has locked in and nobody has settled yet.
    pub fn held_claimable(&self) -> Result<Vec<Held>> {
        let c = self.conn();
        let mut st = c.prepare(&format!("SELECT {HELD_COLS} FROM held WHERE state IN ('claimable', 'forwarding')"))?;
        let rows = st.query_map([], held_row)?.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The held payment whose forward to the phone is payment `forward_id`.
    pub fn held_by_forward(&self, forward_id: [u8; 32]) -> Result<Option<Held>> {
        let c = self.conn();
        Ok(c.query_row(
            &format!("SELECT {HELD_COLS} FROM held WHERE forward_id = ?1"),
            params![forward_id.to_vec()],
            held_row,
        )
        .optional()?)
    }

    /// Moves a held payment to `state` (`claimable`, `forwarding`, `done`,
    /// `credited`, `failed`), recording what is known.
    pub fn held_update(
        &self,
        hash: [u8; 32],
        state: &str,
        claimable: Option<(u64, Option<u32>)>,
        forward_id: Option<Option<[u8; 32]>>,
    ) -> Result<()> {
        let c = self.conn();
        c.execute("UPDATE held SET state = ?2 WHERE payment_hash = ?1", params![hash.to_vec(), state])?;
        if let Some((amount, deadline)) = claimable {
            c.execute(
                "UPDATE held SET claimable_msat = ?2, deadline = ?3 WHERE payment_hash = ?1",
                params![hash.to_vec(), as_i64(amount), deadline.map(|d| d as i64)],
            )?;
        }
        if let Some(f) = forward_id {
            c.execute(
                "UPDATE held SET forward_id = ?2 WHERE payment_hash = ?1",
                params![hash.to_vec(), f.map(|f| f.to_vec())],
            )?;
        }
        Ok(())
    }

    pub fn invoice_owner(&self, payment_hash: [u8; 32]) -> Result<Option<PublicKey>> {
        let c = self.conn();
        let n: Option<String> = c
            .query_row(
                "SELECT node FROM invoices WHERE payment_hash = ?1",
                params![payment_hash.to_vec()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n.and_then(|n| n.parse().ok()))
    }
}

/// A payment the LSP holds for an account's phone (`held.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub payment_hash: [u8; 32],
    pub node: PublicKey,
    pub preimage: [u8; 32],
    pub amount_msat: u64,
    pub claimable_msat: Option<u64>,
    pub deadline: Option<u32>,
    pub forward_id: Option<[u8; 32]>,
    pub state: String,
}

const HELD_COLS: &str = "payment_hash, node, preimage, amount_msat, claimable_msat, deadline, forward_id, state";

fn held_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Held> {
    let b32 = |v: Vec<u8>| -> [u8; 32] { v.try_into().unwrap_or([0; 32]) };
    let node: String = r.get(1)?;
    Ok(Held {
        payment_hash: b32(r.get(0)?),
        node: node.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
        preimage: b32(r.get(2)?),
        amount_msat: r.get::<_, i64>(3)?.max(0) as u64,
        claimable_msat: r.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
        deadline: r.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as u32),
        forward_id: r.get::<_, Option<Vec<u8>>>(6)?.map(b32),
        state: r.get(7)?,
    })
}

fn account_of(c: &Connection, k: &str) -> Result<Account> {
    let balance: Option<i64> = c
        .query_row(
            "SELECT balance_msat FROM accounts WHERE node = ?1",
            params![k],
            |r| r.get(0),
        )
        .optional()?;
    let held: i64 = c.query_row(
        "SELECT COALESCE(SUM(amount_msat), 0) FROM holds WHERE node = ?1",
        params![k],
        |r| r.get(0),
    )?;
    Ok(Account {
        balance_msat: balance.unwrap_or(0).max(0) as u64,
        held_msat: held.max(0) as u64,
    })
}

fn add_balance(c: &Connection, k: &str, delta_msat: i64) -> Result<()> {
    c.execute(
        "INSERT INTO accounts (node, balance_msat) VALUES (?1, MAX(?2, 0))
         ON CONFLICT(node) DO UPDATE SET balance_msat = balance_msat + ?2",
        params![k, delta_msat],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            LedgerError::InsufficientBalance
        }
        e => LedgerError::Db(e),
    })?;
    Ok(())
}

fn take_hold(c: &Connection, id: &str) -> Result<(String, u64)> {
    let row: Option<(String, i64)> = c
        .query_row(
            "SELECT node, amount_msat FROM holds WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (k, amount) = row.ok_or_else(|| LedgerError::UnknownHold(id.to_string()))?;
    c.execute("DELETE FROM holds WHERE id = ?1", params![id])?;
    Ok((k, amount.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ldk_node::bitcoin::secp256k1::{Secp256k1, SecretKey};

    fn pk(n: u8) -> PublicKey {
        PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
    }

    fn ledger(tag: &str) -> (Ledger, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("fortis-lsp-ledger-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LEDGER_FILE);
        (Ledger::open(&path).unwrap(), path)
    }

    #[test]
    fn credit_then_balance() {
        let (l, _) = ledger("credit");
        assert!(l.credit(&pk(1), [1; 32], 100_000).unwrap());
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 100_000,
                held_msat: 0
            }
        );
        assert_eq!(l.balance(&pk(2)).unwrap(), Account::default());
    }

    #[test]
    fn credit_is_idempotent_per_payment_hash() {
        let (l, _) = ledger("idem");
        assert!(l.credit(&pk(1), [1; 32], 100_000).unwrap());
        assert!(!l.credit(&pk(1), [1; 32], 100_000).unwrap());
        assert_eq!(l.balance(&pk(1)).unwrap().balance_msat, 100_000);
    }

    #[test]
    fn hold_settle_and_release() {
        let (l, _) = ledger("hold");
        l.credit(&pk(1), [1; 32], 50_000).unwrap();
        l.hold(&pk(1), "a", 30_000).unwrap();
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 50_000,
                held_msat: 30_000
            }
        );
        assert!(matches!(
            l.hold(&pk(1), "b", 30_000),
            Err(LedgerError::InsufficientBalance)
        ));
        l.settle_hold("a", 21_000, 1_000, Some([9; 32])).unwrap();
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 29_000,
                held_msat: 0
            }
        );
        l.hold(&pk(1), "c", 10_000).unwrap();
        l.settle_hold("c", 0, 0, None).unwrap();
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 29_000,
                held_msat: 0
            }
        );
        assert!(matches!(
            l.settle_hold("c", 0, 0, None),
            Err(LedgerError::UnknownHold(_))
        ));
    }

    #[test]
    fn a_send_debits_amount_plus_actual_fee() {
        let (l, _) = ledger("send");
        l.credit(&pk(1), [1; 32], 100_000).unwrap();
        l.hold_send(&pk(1), "s", 30_000, 30_000 + 300 + 50_000)
            .unwrap();
        assert_eq!(l.balance(&pk(1)).unwrap().free_msat(), 19_700);
        l.settle_send("s", 1_000, Some([4; 32])).unwrap();
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 69_000,
                held_msat: 0
            }
        );
        let h = l.history(&pk(1), 1).unwrap();
        assert_eq!(
            (h[0].kind, h[0].amount_msat, h[0].fee_msat),
            (EntryKind::Sent, -31_000, 1_000)
        );
    }

    #[test]
    fn transfer_moves_atomically_and_once() {
        let (l, _) = ledger("transfer");
        l.credit(&pk(1), [1; 32], 10_000).unwrap();
        assert!(l.transfer(&pk(1), &pk(2), [7; 32], 4_000).unwrap());
        assert!(!l.transfer(&pk(1), &pk(2), [7; 32], 4_000).unwrap());
        assert_eq!(l.balance(&pk(1)).unwrap().balance_msat, 6_000);
        assert_eq!(l.balance(&pk(2)).unwrap().balance_msat, 4_000);
        // Not enough: nothing moves.
        assert!(matches!(
            l.transfer(&pk(1), &pk(2), [8; 32], 100_000),
            Err(LedgerError::InsufficientBalance)
        ));
        assert_eq!(l.balance(&pk(1)).unwrap().balance_msat, 6_000);
        assert_eq!(l.total_custody_msat().unwrap(), 10_000);
    }

    #[test]
    fn conversion_debits_only_when_finished() {
        let (l, _) = ledger("conv");
        l.credit(&pk(1), [1; 32], 55_000_000).unwrap();
        assert_eq!(l.start_conversion(&pk(1), "x").unwrap(), 55_000_000);
        assert_eq!(l.balance(&pk(1)).unwrap().free_msat(), 0);
        l.abandon_conversion("x").unwrap();
        assert_eq!(
            l.balance(&pk(1)).unwrap(),
            Account {
                balance_msat: 55_000_000,
                held_msat: 0
            }
        );
        l.start_conversion(&pk(1), "y").unwrap();
        assert_eq!(l.conversions().unwrap(), vec![("y".to_string(), pk(1))]);
        l.finish_conversion("y", 1_000_000).unwrap();
        assert_eq!(l.balance(&pk(1)).unwrap(), Account::default());
        let h = l.history(&pk(1), 10).unwrap();
        assert_eq!(h[0].kind, EntryKind::Converted);
        assert_eq!((h[0].amount_msat, h[0].fee_msat), (-55_000_000, 1_000_000));
    }

    #[test]
    fn custody_and_over_cap() {
        let (l, _) = ledger("custody");
        l.credit(&pk(1), [1; 32], 60_000_000).unwrap();
        l.credit(&pk(2), [2; 32], 1_000).unwrap();
        l.hold(&pk(2), "h", 500).unwrap();
        assert_eq!(l.total_custody_msat().unwrap(), 60_001_000);
        let over = l.over_cap(50_000_000).unwrap();
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].0, pk(1));
    }

    #[test]
    fn history_is_newest_first() {
        let (l, _) = ledger("hist");
        l.credit(&pk(1), [1; 32], 1_000).unwrap();
        l.credit(&pk(1), [2; 32], 2_000).unwrap();
        let h = l.history(&pk(1), 10).unwrap();
        assert_eq!(
            h.iter().map(|e| e.amount_msat).collect::<Vec<_>>(),
            vec![2_000, 1_000]
        );
        assert_eq!(h[0].payment_hash, Some([2; 32]));
    }

    #[test]
    fn nonce_is_single_use_and_pruned_after_expiry() {
        let (l, _) = ledger("nonce");
        let later = now_secs() + 120;
        assert!(l.use_nonce(&pk(1), "abc", later).unwrap());
        assert!(!l.use_nonce(&pk(1), "abc", later).unwrap());
        assert!(l.use_nonce(&pk(2), "abc", later).unwrap(), "per node");
        assert!(l.use_nonce(&pk(1), "old", 1).unwrap());
        // The next call prunes "old" (expired), so it can be inserted again.
        assert!(l.use_nonce(&pk(1), "old", 1).unwrap());
    }

    #[test]
    fn invoice_owner_is_recorded() {
        let (l, _) = ledger("inv");
        l.pending_invoice([5; 32], &pk(3), 100, now_secs() + 3600)
            .unwrap();
        assert_eq!(l.invoice_owner([5; 32]).unwrap(), Some(pk(3)));
        assert_eq!(l.invoice_owner([6; 32]).unwrap(), None);
    }

    #[test]
    fn ledger_file_is_owner_only_and_survives_reopen() {
        let (l, path) = ledger("file");
        l.credit(&pk(1), [1; 32], 7_000).unwrap();
        drop(l);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.balance(&pk(1)).unwrap().balance_msat, 7_000);
    }
}
