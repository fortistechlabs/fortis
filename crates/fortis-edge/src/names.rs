//! Lightning address names (`name@fortis.cash`, LUD-16): the rules a name must meet and the SQLite file
//! that says which wallet holds which. One list for both chains; the database, not application code,
//! decides who got a name first (primary key on the name) and that a wallet holds at most one (unique
//! index on chain + node id).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, ErrorCode, TransactionBehavior};
use serde_json::{json, Value};

/// Names nobody may claim (they would pass for the operator or for a service).
const RESERVED: &[&str] = &[
    "abuse", "admin", "administrator", "api", "billing", "bitcoin", "btc", "contact", "dns", "ftp", "fortis", "help",
    "hostmaster", "imap", "info", "lightning", "lnurlp", "mail", "no-reply", "noreply", "postmaster", "pop", "root",
    "sales", "satoshi", "security", "smtp", "staff", "status", "support", "team", "wallet", "webmaster", "www", "xbt",
];

pub const MIN_LEN: usize = 3;
pub const MAX_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Not 3 to 32 characters.
    Length,
    /// A character outside `a-z 0-9 . _ -` (an uppercase letter included: the app lowercases, the server refuses).
    Chars,
    /// Does not start and end with a letter or digit.
    Edges,
    /// Contains `..`.
    DoubleDot,
    Reserved,
}

impl NameError {
    pub fn as_str(self) -> &'static str {
        match self {
            NameError::Length => "length",
            NameError::Chars => "characters",
            NameError::Edges => "edges",
            NameError::DoubleDot => "double_dot",
            NameError::Reserved => "reserved",
        }
    }
}

/// Whether `name` may be claimed.
pub fn check_name(name: &str) -> Result<(), NameError> {
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')) {
        return Err(NameError::Chars);
    }
    if !(MIN_LEN..=MAX_LEN).contains(&name.len()) {
        return Err(NameError::Length);
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let b = name.as_bytes();
    if !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return Err(NameError::Edges);
    }
    if name.contains("..") {
        return Err(NameError::DoubleDot);
    }
    if RESERVED.contains(&name) {
        return Err(NameError::Reserved);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Btc,
    Xbt,
}

impl Chain {
    pub fn as_str(self) -> &'static str {
        match self {
            Chain::Btc => "btc",
            Chain::Xbt => "xbt",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Chain::Btc => "BTC",
            Chain::Xbt => "XBT",
        }
    }
    pub fn parse(s: &str) -> Option<Chain> {
        match s {
            "btc" => Some(Chain::Btc),
            "xbt" => Some(Chain::Xbt),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClaimError {
    Invalid(NameError),
    /// Someone else holds the name.
    Taken,
    /// This wallet already holds a name (release it first).
    HasName,
    Db(String),
}

pub struct Names {
    conn: Mutex<Connection>,
}

impl Names {
    /// Opens (creating, owner-only) the names file.
    pub fn open(path: &Path) -> Result<Names, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| format!("names file: {e}"))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("names file: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS names (
                 name TEXT PRIMARY KEY,
                 chain TEXT NOT NULL CHECK (chain IN ('btc','xbt')),
                 node_id TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS names_owner ON names (chain, node_id);",
        )
        .map_err(|e| format!("names file: {e}"))?;
        Ok(Names { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Gives `name` to the wallet `node_id` on `chain`.
    pub fn claim(&self, name: &str, chain: Chain, node_id: &str, now: u64) -> Result<(), ClaimError> {
        check_name(name).map_err(ClaimError::Invalid)?;
        let mut c = self.conn();
        let db = |e: rusqlite::Error| ClaimError::Db(e.to_string());
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db)?;
        // A wallet that claims the name it already holds (a retry after a lost answer) has it.
        match Self::name_of_in(&tx, chain, node_id).map_err(ClaimError::Db)? {
            Some(held) if held == name => return Ok(()),
            Some(_) => return Err(ClaimError::HasName),
            None => {}
        }
        match tx.execute(
            "INSERT INTO names (name, chain, node_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, chain.as_str(), node_id, now as i64],
        ) {
            Ok(_) => tx.commit().map_err(db),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::ConstraintViolation => Err(ClaimError::Taken),
            Err(e) => Err(db(e)),
        }
    }

    /// Frees the name `node_id` holds on `chain` (only that wallet's own); the name it held.
    pub fn release(&self, chain: Chain, node_id: &str) -> Result<Option<String>, String> {
        let mut c = self.conn();
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
        let name = Self::name_of_in(&tx, chain, node_id)?;
        if name.is_some() {
            tx.execute("DELETE FROM names WHERE chain = ?1 AND node_id = ?2", params![chain.as_str(), node_id])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(name)
    }

    fn name_of_in(c: &Connection, chain: Chain, node_id: &str) -> Result<Option<String>, String> {
        match c.query_row("SELECT name FROM names WHERE chain = ?1 AND node_id = ?2", params![chain.as_str(), node_id], |r| r.get(0)) {
            Ok(n) => Ok(Some(n)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The name `node_id` holds on `chain`.
    pub fn name_of(&self, chain: Chain, node_id: &str) -> Result<Option<String>, String> {
        Self::name_of_in(&self.conn(), chain, node_id)
    }

    /// Who holds `name`: its chain and wallet.
    pub fn lookup(&self, name: &str) -> Result<Option<(Chain, String)>, String> {
        match self.conn().query_row("SELECT chain, node_id FROM names WHERE name = ?1", params![name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok((chain, node)) => Ok(Chain::parse(&chain).map(|c| (c, node))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The three signed operations on names a wallet sends through `/ln/account`.
pub const NAME_OPS: [&str; 3] = ["claim_name", "release_name", "my_name"];

/// The wallet id as the LSP spells it (lowercase hex of a compressed key): 66 hex digits.
fn node_hex(v: &Value) -> Option<String> {
    v.as_str().filter(|n| n.len() == 66 && n.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_ascii_lowercase)
}

/// Carries out the name operation `op` for `chain`. `request` is the body the wallet signed and `verified`
/// what the LSP answered after checking the signature (`{"node_id","op","params"}`): the operation is
/// applied only when the two agree on who, what and which chain, so nothing the LSP's answer adds or
/// changes can move a name. The status and body to answer the wallet with.
pub fn apply(names: &Names, chain: Chain, op: &str, request: &Value, verified: &Value, now: u64) -> (u16, Value) {
    let bad = || (502, json!({ "error": "bad_upstream_answer" }));
    let (Some(node), Some(asked)) = (node_hex(&verified["node_id"]), node_hex(&request["node_id"])) else { return bad() };
    if node != asked || verified["op"] != op || request["op"] != op || verified["params"]["name"] != request["params"]["name"] {
        return bad();
    }
    // The signed body says which chain it is for: a copy replayed to the other chain's server is refused.
    if request["params"]["chain"] != chain.as_str() || verified["params"]["chain"] != chain.as_str() {
        return (400, json!({ "error": "wrong_chain" }));
    }
    let node = node.as_str();
    let db = |e: String| {
        eprintln!("fortis-edge: names: {e}");
        (500, json!({ "error": "names" }))
    };
    match op {
        "claim_name" => {
            let Some(name) = verified["params"]["name"].as_str() else {
                return (400, json!({ "error": "name_invalid", "reason": "missing" }));
            };
            match names.claim(name, chain, node, now) {
                Ok(()) => (200, json!({ "name": name })),
                Err(ClaimError::Invalid(e)) => (400, json!({ "error": "name_invalid", "reason": e.as_str() })),
                Err(ClaimError::Taken) => (409, json!({ "error": "name_taken" })),
                Err(ClaimError::HasName) => (409, json!({ "error": "has_name" })),
                Err(ClaimError::Db(e)) => db(e),
            }
        }
        "release_name" => match names.release(chain, node) {
            Ok(n) => (200, json!({ "released": n })),
            Err(e) => db(e),
        },
        "my_name" => match names.name_of(chain, node) {
            Ok(n) => (200, json!({ "name": n })),
            Err(e) => db(e),
        },
        _ => (400, json!({ "error": "unknown_op" })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn open(tag: &str) -> (Names, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fortis-edge-names-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("names.sqlite");
        (Names::open(&p).unwrap(), p)
    }

    #[test]
    fn the_name_rules() {
        let ok = ["abc", "donal", "a.b", "a_b-c", "x1y", "0day", &"a".repeat(32)];
        for n in ok {
            assert_eq!(check_name(n), Ok(()), "{n}");
        }
        let bad: [(&str, NameError); 16] = [
            ("ab", NameError::Length),
            ("", NameError::Length),
            (&"a".repeat(33), NameError::Length),
            ("Donal", NameError::Chars),
            ("dön", NameError::Chars),
            ("a b", NameError::Chars),
            ("a@b", NameError::Chars),
            ("a/b", NameError::Chars),
            (".abc", NameError::Edges),
            ("abc.", NameError::Edges),
            ("-abc", NameError::Edges),
            ("abc_", NameError::Edges),
            ("a..b", NameError::DoubleDot),
            ("admin", NameError::Reserved),
            ("lnurlp", NameError::Reserved),
            ("fortis", NameError::Reserved),
        ];
        for (n, e) in bad {
            assert_eq!(check_name(n), Err(e), "{n}");
        }
    }

    #[test]
    fn a_claim_is_stored_and_found_by_name_and_by_wallet() {
        let (n, _) = open("claim");
        n.claim("donal", Chain::Xbt, "02aa", 1).unwrap();
        assert_eq!(n.lookup("donal").unwrap(), Some((Chain::Xbt, "02aa".to_string())));
        assert_eq!(n.name_of(Chain::Xbt, "02aa").unwrap(), Some("donal".to_string()));
        // The same wallet key on the other chain is another wallet.
        assert_eq!(n.name_of(Chain::Btc, "02aa").unwrap(), None);
        assert_eq!(n.lookup("nobody").unwrap(), None);
    }

    #[test]
    fn a_taken_name_and_a_second_name_are_refused() {
        let (n, _) = open("refuse");
        n.claim("donal", Chain::Btc, "02aa", 1).unwrap();
        // Names are unique across both chains.
        assert_eq!(n.claim("donal", Chain::Xbt, "02bb", 2), Err(ClaimError::Taken));
        assert_eq!(n.claim("donal", Chain::Btc, "02bb", 2), Err(ClaimError::Taken));
        // A wallet holds one name per chain.
        assert_eq!(n.claim("other", Chain::Btc, "02aa", 3), Err(ClaimError::HasName));
        n.claim("other", Chain::Xbt, "02aa", 3).unwrap();
        assert_eq!(n.claim("Bad", Chain::Btc, "02cc", 4), Err(ClaimError::Invalid(NameError::Chars)));
        assert_eq!(n.lookup("other").unwrap().unwrap().0, Chain::Xbt);
    }

    #[test]
    fn only_the_owner_releases_and_a_released_name_is_free_at_once() {
        let (n, _) = open("release");
        n.claim("donal", Chain::Btc, "02aa", 1).unwrap();
        // Another wallet, or the same key on the other chain, releases nothing.
        assert_eq!(n.release(Chain::Btc, "02bb").unwrap(), None);
        assert_eq!(n.release(Chain::Xbt, "02aa").unwrap(), None);
        assert!(n.lookup("donal").unwrap().is_some());
        assert_eq!(n.release(Chain::Btc, "02aa").unwrap(), Some("donal".to_string()));
        assert_eq!(n.lookup("donal").unwrap(), None);
        // Free for anyone, at once, including another chain.
        n.claim("donal", Chain::Xbt, "02bb", 2).unwrap();
    }

    #[test]
    fn two_claims_of_one_name_at_once_give_it_to_exactly_one() {
        let (n, p) = open("race");
        drop(n);
        let mut handles = Vec::new();
        let go = Arc::new(std::sync::Barrier::new(8));
        for i in 0..8 {
            let p = p.clone();
            let go = go.clone();
            handles.push(std::thread::spawn(move || {
                // Each with its own connection, as separate processes would be.
                let n = Names::open(&p).unwrap();
                go.wait();
                n.claim("race", if i % 2 == 0 { Chain::Btc } else { Chain::Xbt }, &format!("02{i:02}"), 1)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
        assert!(results.iter().filter(|r| r.is_err()).all(|r| *r == Err(ClaimError::Taken)), "{results:?}");
    }

    #[test]
    fn names_survive_a_restart() {
        let (n, p) = open("restart");
        n.claim("donal", Chain::Btc, "02aa", 1).unwrap();
        drop(n);
        let n = Names::open(&p).unwrap();
        assert_eq!(n.name_of(Chain::Btc, "02aa").unwrap(), Some("donal".to_string()));
    }


    /// A wallet's signed request and the LSP's verified echo of it.
    fn pair(chain: Chain, node: &str, op: &str, name: Option<&str>) -> (Value, Value) {
        let mut params = json!({ "chain": chain.as_str() });
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        let req = json!({ "node_id": node, "op": op, "params": params, "nonce": "n".repeat(20), "expires_at": 1 });
        let echo = json!({ "node_id": node, "op": op, "params": req["params"] });
        (req, echo)
    }

    const A: &str = "02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn name_operations_act_for_the_wallet_the_lsp_verified() {
        let (n, _) = open("apply");
        let go = |chain, op, node, name| {
            let (req, echo) = pair(chain, node, op, name);
            apply(&n, chain, op, &req, &echo, 1)
        };
        assert_eq!(go(Chain::Btc, "my_name", A, None), (200, json!({ "name": null })));
        assert_eq!(go(Chain::Btc, "claim_name", A, Some("donal")), (200, json!({ "name": "donal" })));
        assert_eq!(go(Chain::Btc, "my_name", A, None), (200, json!({ "name": "donal" })));
        // Claiming the name it already holds again (a retry after a lost answer) works.
        assert_eq!(go(Chain::Btc, "claim_name", A, Some("donal")), (200, json!({ "name": "donal" })));
        // Taken by another wallet, on either chain; a second name for the same wallet.
        assert_eq!(go(Chain::Xbt, "claim_name", B, Some("donal")), (409, json!({ "error": "name_taken" })));
        assert_eq!(go(Chain::Btc, "claim_name", A, Some("second")), (409, json!({ "error": "has_name" })));
        assert_eq!(go(Chain::Btc, "claim_name", B, Some("Bad")), (400, json!({ "error": "name_invalid", "reason": "characters" })));
        assert_eq!(go(Chain::Btc, "claim_name", B, None).0, 400);
        // Another wallet cannot release it; the owner can; then it is free.
        assert_eq!(go(Chain::Btc, "release_name", B, None), (200, json!({ "released": null })));
        assert_eq!(go(Chain::Btc, "release_name", A, None), (200, json!({ "released": "donal" })));
        assert_eq!(go(Chain::Xbt, "claim_name", B, Some("donal")), (200, json!({ "name": "donal" })));
    }

    #[test]
    fn an_answer_that_does_not_agree_with_the_signed_request_changes_nothing() {
        let (n, _) = open("apply-bad");
        let (req, echo) = pair(Chain::Btc, A, "claim_name", Some("donal"));
        let with = |k: &str, v: Value| {
            let mut e = echo.clone();
            e[k] = v;
            e
        };
        let mut other_name = echo.clone();
        other_name["params"]["name"] = json!("swapped");
        let mut other_chain = echo.clone();
        other_chain["params"]["chain"] = json!("xbt");
        for e in [
            with("op", json!("my_name")),
            with("op", json!("CLAIM_NAME")),
            with("node_id", json!(B)),                       // another wallet
            with("node_id", json!("nothex")),
            with("node_id", json!("0".repeat(65))),
            with("node_id", Value::Null),
            other_name,                                      // another name
            json!({ "op": "claim_name", "params": { "name": "donal", "chain": "btc" } }),
            Value::Null,
        ] {
            assert_eq!(apply(&n, Chain::Btc, "claim_name", &req, &e, 1).0, 502, "{e}");
        }
        // An echo that names another chain than the request signed is refused as a chain error.
        assert_eq!(apply(&n, Chain::Btc, "claim_name", &req, &other_chain, 1), (400, json!({ "error": "wrong_chain" })));
        assert_eq!(n.lookup("donal").unwrap(), None);
        assert_eq!(n.lookup("swapped").unwrap(), None);
        // The request itself must be well formed too.
        assert_eq!(apply(&n, Chain::Btc, "claim_name", &Value::Null, &echo, 1).0, 502);
        assert_eq!(apply(&n, Chain::Btc, "other_op", &req, &echo, 1).0, 502);
    }

    #[test]
    fn a_request_signed_for_another_chain_is_refused() {
        let (n, _) = open("apply-chain");
        // Signed for the BTC server, replayed to the XBT one (the echo agrees, as the LSP only checks the signature).
        let (req, echo) = pair(Chain::Btc, A, "claim_name", Some("donal"));
        assert_eq!(apply(&n, Chain::Xbt, "claim_name", &req, &echo, 1), (400, json!({ "error": "wrong_chain" })));
        // No chain signed at all.
        let mut bare = req.clone();
        bare["params"].as_object_mut().unwrap().remove("chain");
        let mut bare_echo = echo.clone();
        bare_echo["params"].as_object_mut().unwrap().remove("chain");
        assert_eq!(apply(&n, Chain::Btc, "claim_name", &bare, &bare_echo, 1).0, 400);
        assert_eq!(n.lookup("donal").unwrap(), None);
    }

    #[test]
    fn a_wallet_id_in_another_case_is_the_same_wallet() {
        let (n, _) = open("apply-case");
        let (req, echo) = pair(Chain::Btc, A, "claim_name", Some("first1"));
        assert_eq!(apply(&n, Chain::Btc, "claim_name", &req, &echo, 1).0, 200);
        let upper = A.to_ascii_uppercase();
        let (req2, echo2) = pair(Chain::Btc, &upper, "claim_name", Some("second1"));
        assert_eq!(apply(&n, Chain::Btc, "claim_name", &req2, &echo2, 1), (409, json!({ "error": "has_name" })));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_n, p) = open("mode");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
