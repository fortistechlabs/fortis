//! The one-active-device guard: a `device-lock` record in the node's own
//! VSS store, written with VSS conditional versions (spec, Findings (Task 1)
//! (b): ldk-node 0.7.1 writes every key unconditionally, so two devices on
//! one seed would silently overwrite each other's channel state).
//!
//! **Device id.** 16 random bytes, hex, in `<data_dir>/fortis-device-id`
//! (written atomically and durably, mode 0600). Wiping the data dir makes a
//! new device. A file that is not 32 hex characters is an error
//! (`Other("device_id_corrupt")`), never replaced silently: a new id would
//! make this device "another device" for its own lock.
//!
//! **Record.** Key [`LOCK_KEY`] = `fortis/device-lock` in the node's store
//! (same store id, same `Authorization` header as ldk-node). Value: JSON
//! `{"device_id":"<32 hex>","since":<unix secs>}`, plus the VSS version of
//! the key. Plain JSON, not encrypted: it holds a random id and a time, no
//! seed material, and it is as authenticated as the rest of the store —
//! whoever can write it (store id + VSS auth) can also overwrite the
//! channel state itself, so a MAC on the lock would add nothing.
//!
//! **Why the key cannot collide with ldk-node's.** ldk-node 0.7.1's
//! `io/vss_store.rs` stores (schema V1, what a new store gets)
//! `obfuscate(primary#secondary) + "#" + obfuscate(key)`, the legacy V0
//! schema `primary#secondary#obfuscate(key)`, or a bare `obfuscate(key)`
//! for an empty primary namespace; `obfuscate` is `BASE64_STANDARD_NO_PAD`
//! (alphabet `A–Z a–z 0–9 + /`) of an encrypted blob. Every V0/V1 key with
//! a namespace contains `#`; a bare one contains no `-`.
//! `fortis/device-lock` has a `-` and no `#`, so it is none of them, and it
//! matches none of ldk-node's list prefixes (an obfuscated prefix, `p#s`,
//! or the V0 probe `bdk_wallet#`). Its schema marker `vss_schema_version`
//! is itself a V1 key. The reachability probe key `fortis-probe` (never
//! written) is not either.
//!
//! **Versions** (vss-server): version 0 = insert-if-absent (stored as 1);
//! version n > 0 = write only if the stored version is n (then n + 1);
//! a mismatch is `ConflictError`. Conditional deletes go through
//! `putObjects` `delete_items` (a plain `deleteObject` of a mismatched
//! version answers success, so it could not tell).
//!
//! **Failure directions.** Unreachable / timed out / unexpected answer →
//! `BackupUnavailable`; another holder → `ActiveOnAnotherDevice`; a record
//! that does not parse → `Other("device_lock_corrupt")` (only
//! [`takeover`] overwrites it). Nothing here ever proceeds on doubt.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vss_client::client::VssClient;
use vss_client::error::VssError;
use vss_client::headers::FixedHeaders;
use vss_client::types::{GetObjectRequest, KeyValue, ListKeyVersionsRequest, PutObjectRequest};
use vss_client::util::retry::{ExponentialBackoffRetryPolicy, MaxAttemptsRetryPolicy, RetryPolicy};

use crate::{LnError, VssConfig};

/// The lock's key in the node's VSS store.
pub(crate) const LOCK_KEY: &str = "fortis/device-lock";
const DEVICE_ID_FILE: &str = "fortis-device-id";

/// This device's id: random, local to the data dir. Not secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceId([u8; 16]);

impl DeviceId {
    #[cfg(test)]
    pub(crate) const TEST: DeviceId = DeviceId([0xEE; 16]);

    fn hex(&self) -> String {
        hex::encode(self.0)
    }

    fn from_hex(s: &str) -> Option<DeviceId> {
        if s.len() != 32 || !s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut b = [0u8; 16];
        hex::decode_to_slice(s, &mut b).ok()?;
        Some(DeviceId(b))
    }
}

/// The id in `dir`, created on first use.
pub(crate) fn load_or_create_device_id(dir: &Path) -> Result<DeviceId, LnError> {
    let path = dir.join(DEVICE_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => DeviceId::from_hex(&s).ok_or_else(|| LnError::Other("device_id_corrupt".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = [0u8; 16];
            getrandom::getrandom(&mut b).map_err(|e| LnError::Other(format!("device id: {e}")))?;
            let id = DeviceId(b);
            crate::pay::write_atomic(dir, DEVICE_ID_FILE, id.hex().as_bytes())
                .map_err(|e| LnError::Other(format!("device id: {e}")))?;
            Ok(id)
        }
        Err(e) => Err(LnError::Other(format!("device id: {e}"))),
    }
}

/// The lock's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockRecord {
    pub(crate) device_id: DeviceId,
    /// When this device took the lock (unix seconds). Informational.
    pub(crate) since: u64,
}

impl LockRecord {
    pub(crate) fn encode(&self) -> Vec<u8> {
        serde_json::json!({ "device_id": self.device_id.hex(), "since": self.since })
            .to_string()
            .into_bytes()
    }

    /// Strict: an object with a 32-lowercase-hex `device_id` and a u64
    /// `since` (other fields are ignored, for a later version's additions).
    pub(crate) fn decode(b: &[u8]) -> Option<LockRecord> {
        let v: serde_json::Value = serde_json::from_slice(b).ok()?;
        let device_id = DeviceId::from_hex(v.get("device_id")?.as_str()?)?;
        let since = v.get("since")?.as_u64()?;
        Some(LockRecord { device_id, since })
    }
}

/// What this node knows it holds: the record's VSS version after its own
/// last write, and its `since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) version: i64,
    pub(crate) since: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreError {
    /// VSS refused a conditional write (version mismatch).
    Conflict,
    /// No answer in time, transport failure, auth refused, anything else.
    Unavailable,
}

/// The lock record's storage (VSS in production, a fake in unit tests).
pub(crate) trait LockStore {
    /// The record's bytes and version, `None` when absent.
    fn get(&self) -> Result<Option<(Vec<u8>, i64)>, StoreError>;
    /// Conditional write at `version` (0 = insert-if-absent).
    fn put(&self, value: Vec<u8>, version: i64) -> Result<(), StoreError>;
    /// Conditional delete at `version`.
    fn delete(&self, version: i64) -> Result<(), StoreError>;
    /// Whether the store holds any key other than [`LOCK_KEY`].
    fn has_other_keys(&self) -> Result<bool, StoreError>;
}

fn corrupt() -> LnError {
    LnError::Other("device_lock_corrupt".into())
}

fn unavailable(_: StoreError) -> LnError {
    LnError::BackupUnavailable
}

/// Whether a check's error means this node no longer holds the lock (stop
/// the node), as opposed to "could not tell" (`BackupUnavailable`: keep
/// running, refuse the action).
pub(crate) fn is_lost(e: &LnError) -> bool {
    *e == LnError::ActiveOnAnotherDevice || *e == corrupt()
}

/// Start (both modes): absent → insert at version 0; ours (a crash
/// restart of this device) → rewrite at the version read; another
/// device's → `ActiveOnAnotherDevice`, nothing written; unparsable →
/// `Other("device_lock_corrupt")`, nothing written. A conflict between the
/// read and the write (someone wrote in between) is re-read once.
pub(crate) fn acquire(store: &dyn LockStore, me: DeviceId, now: u64) -> Result<Held, LnError> {
    for _ in 0..2 {
        let (version, since) = match store.get().map_err(unavailable)? {
            None => (0, now),
            Some((bytes, v)) => {
                let rec = LockRecord::decode(&bytes).ok_or_else(corrupt)?;
                if rec.device_id != me {
                    return Err(LnError::ActiveOnAnotherDevice);
                }
                (v, now)
            }
        };
        let rec = LockRecord { device_id: me, since };
        match store.put(rec.encode(), version) {
            Ok(()) => return Ok(Held { version: version + 1, since }),
            Err(StoreError::Conflict) => continue,
            Err(e) => return Err(unavailable(e)),
        }
    }
    Err(LnError::ActiveOnAnotherDevice)
}

/// Takeover: reads the record (any holder, also an unparsable one) and
/// writes ours at the version read, so of two concurrent takeovers (or a
/// takeover and the old holder's refresh) exactly one write lands; the
/// other gets `ActiveOnAnotherDevice`. `after_read` runs between the two
/// round trips (a test hook).
pub(crate) fn takeover(
    store: &dyn LockStore,
    me: DeviceId,
    now: u64,
    after_read: impl FnOnce(),
) -> Result<Held, LnError> {
    let version = store.get().map_err(unavailable)?.map_or(0, |(_, v)| v);
    after_read();
    let rec = LockRecord { device_id: me, since: now };
    match store.put(rec.encode(), version) {
        Ok(()) => Ok(Held { version: version + 1, since: now }),
        Err(StoreError::Conflict) => Err(LnError::ActiveOnAnotherDevice),
        Err(e) => Err(unavailable(e)),
    }
}

/// The running node's check: rewrites our record at the version we hold
/// (one round trip; doubles as the VSS reachability probe). On a conflict
/// the record is re-read: still ours at a newer version means an earlier
/// write of ours landed after its answer was lost (a timeout) — adopt it
/// into `held` and write again; anything else (another device's, gone,
/// unparsable) means the lock is lost ([`is_lost`]).
pub(crate) fn refresh(
    store: &dyn LockStore,
    me: DeviceId,
    held: &mut Held,
) -> Result<Held, LnError> {
    let rec = LockRecord { device_id: me, since: held.since };
    match store.put(rec.encode(), held.version) {
        Ok(()) => {
            held.version += 1;
            return Ok(*held);
        }
        Err(StoreError::Conflict) => {}
        Err(e) => return Err(unavailable(e)),
    }
    let Some((bytes, v)) = store.get().map_err(unavailable)? else {
        return Err(LnError::ActiveOnAnotherDevice);
    };
    let found = LockRecord::decode(&bytes).ok_or_else(corrupt)?;
    if found.device_id != me {
        return Err(LnError::ActiveOnAnotherDevice);
    }
    held.version = v;
    match store.put(rec.encode(), v) {
        Ok(()) => {
            held.version = v + 1;
            Ok(*held)
        }
        Err(StoreError::Conflict) => Err(LnError::ActiveOnAnotherDevice),
        Err(e) => Err(unavailable(e)),
    }
}

/// Clean stop: deletes the record at the version we hold. A conflict means
/// it is not ours any more (left alone, `Ok`); unreachable is
/// `BackupUnavailable` and the lock stays ours (same device restarts
/// without a takeover; another device needs one).
pub(crate) fn release(store: &dyn LockStore, held: Held) -> Result<(), LnError> {
    match store.delete(held.version) {
        Ok(()) | Err(StoreError::Conflict) => Ok(()),
        Err(e) => Err(unavailable(e)),
    }
}

/// Whether the store holds node state: any key but the lock.
pub(crate) fn state_exists(store: &dyn LockStore) -> Result<bool, LnError> {
    store.has_other_keys().map_err(unavailable)
}

/// Reusable Tokio runtime and [`VssClient`] for a node's VSS lock calls,
/// keeping HTTP keep-alive connections and TLS sessions warm across checks.
pub(crate) struct VssLockClient {
    rt: ldk_node::tokio::runtime::Runtime,
    client: VssClient<MaxAttemptsRetryPolicy<ExponentialBackoffRetryPolicy<VssError>>>,
    store_id: String,
}

impl VssLockClient {
    pub(crate) fn new(vss: &VssConfig, store_id: &str) -> Result<VssLockClient, LnError> {
        let rt = ldk_node::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LnError::Other(format!("runtime: {e}")))?;
        let http = vss_client::reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| LnError::Other(format!("http client: {e}")))?;
        let policy = ExponentialBackoffRetryPolicy::<VssError>::new(Duration::from_millis(10))
            .with_max_attempts(1);
        let client = VssClient::from_client_and_headers(
            vss.url.clone(),
            http,
            policy,
            Arc::new(FixedHeaders::new(vss_headers(vss))),
        );
        Ok(VssLockClient {
            rt,
            client,
            store_id: store_id.to_string(),
        })
    }

    pub(crate) fn with_timeout(&self, timeout: Duration) -> VssLockStore<'_> {
        VssLockStore {
            inner: self,
            deadline: Instant::now() + timeout,
        }
    }
}

/// [`LockStore`] over a [`VssLockClient`] with every call bounded by one
/// shared per-operation deadline. No retries.
pub(crate) struct VssLockStore<'a> {
    inner: &'a VssLockClient,
    deadline: Instant,
}

impl VssLockStore<'_> {
    fn run<T>(&self, fut: impl std::future::Future<Output = Result<T, VssError>>) -> Result<T, VssError> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(VssError::InternalServerError("deadline".into()));
        }
        // The timer is made inside the runtime (it needs its reactor).
        self.inner
            .rt
            .block_on(async { ldk_node::tokio::time::timeout(left, fut).await })
            .unwrap_or_else(|_| Err(VssError::InternalServerError("timeout".into())))
    }

    fn put_request(&self, put: Vec<KeyValue>, delete: Vec<KeyValue>) -> PutObjectRequest {
        PutObjectRequest {
            store_id: self.inner.store_id.clone(),
            global_version: None,
            transaction_items: put,
            delete_items: delete,
        }
    }
}

/// `ConflictError` is a conflict; every other VSS error (and no answer) is
/// "unavailable".
pub(crate) fn map_vss_error(e: &VssError) -> StoreError {
    match e {
        VssError::ConflictError(_) => StoreError::Conflict,
        _ => StoreError::Unavailable,
    }
}

impl LockStore for VssLockStore<'_> {
    fn get(&self) -> Result<Option<(Vec<u8>, i64)>, StoreError> {
        let req = GetObjectRequest {
            store_id: self.inner.store_id.clone(),
            key: LOCK_KEY.into(),
        };
        match self.run(self.inner.client.get_object(&req)) {
            Ok(r) => match r.value {
                Some(kv) if kv.key == LOCK_KEY => Ok(Some((kv.value, kv.version))),
                _ => Err(StoreError::Unavailable),
            },
            Err(VssError::NoSuchKeyError(_)) => Ok(None),
            // A conflict on a read is not a meaningful answer.
            Err(_) => Err(StoreError::Unavailable),
        }
    }

    fn put(&self, value: Vec<u8>, version: i64) -> Result<(), StoreError> {
        let kv = KeyValue {
            key: LOCK_KEY.into(),
            version,
            value,
        };
        let req = self.put_request(vec![kv], vec![]);
        self.run(self.inner.client.put_object(&req))
            .map(|_| ())
            .map_err(|e| map_vss_error(&e))
    }

    fn delete(&self, version: i64) -> Result<(), StoreError> {
        let kv = KeyValue {
            key: LOCK_KEY.into(),
            version,
            value: vec![],
        };
        let req = self.put_request(vec![], vec![kv]);
        self.run(self.inner.client.put_object(&req))
            .map(|_| ())
            .map_err(|e| map_vss_error(&e))
    }

    fn has_other_keys(&self) -> Result<bool, StoreError> {
        // The lock is the only key fortis writes: of any two keys, one is
        // node state (or if the server paged after the first key, more keys
        // follow).
        let req = ListKeyVersionsRequest {
            store_id: self.inner.store_id.clone(),
            key_prefix: None,
            page_size: Some(2),
            page_token: None,
        };
        match self.run(self.inner.client.list_key_versions(&req)) {
            Ok(r) => Ok(r.key_versions.iter().any(|kv| kv.key != LOCK_KEY)
                || r.next_page_token.as_deref().is_some_and(|t| !t.is_empty())),
            Err(_) => Err(StoreError::Unavailable),
        }
    }
}

/// ldk-node's fixed headers for `vss`: `Authorization` when set.
pub(crate) fn vss_headers(vss: &VssConfig) -> std::collections::HashMap<String, String> {
    let mut h = std::collections::HashMap::new();
    if !vss.auth_header.is_empty() {
        h.insert("Authorization".to_string(), vss.auth_header.clone());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    type BeforePut = Box<dyn FnOnce(&Fake)>;

    /// An in-memory VSS key with vss-server's version rules.
    #[derive(Default)]
    struct Fake {
        rec: RefCell<Option<(Vec<u8>, i64)>>,
        other_keys: Cell<bool>,
        /// The next `n` calls answer `Unavailable` without effect.
        down: Cell<u32>,
        /// The next write is applied, but answers `Unavailable` (its
        /// response was lost).
        lose_response: Cell<bool>,
        writes: Cell<u32>,
        /// Runs before the next `put` is applied (a concurrent writer).
        before_put: RefCell<Option<BeforePut>>,
    }

    impl Fake {
        fn holding(id: DeviceId, version: i64) -> Fake {
            let f = Fake::default();
            f.set(Some((LockRecord { device_id: id, since: 7 }.encode(), version)));
            f
        }

        fn set(&self, r: Option<(Vec<u8>, i64)>) {
            *self.rec.borrow_mut() = r;
        }

        fn holder(&self) -> Option<(DeviceId, i64)> {
            self.rec
                .borrow()
                .as_ref()
                .map(|(b, v)| (LockRecord::decode(b).unwrap().device_id, *v))
        }

        fn gate(&self) -> Result<(), StoreError> {
            if self.down.get() > 0 {
                self.down.set(self.down.get() - 1);
                return Err(StoreError::Unavailable);
            }
            Ok(())
        }

        fn answer(&self) -> Result<(), StoreError> {
            if self.lose_response.replace(false) {
                return Err(StoreError::Unavailable);
            }
            Ok(())
        }
    }

    impl LockStore for Fake {
        fn get(&self) -> Result<Option<(Vec<u8>, i64)>, StoreError> {
            self.gate()?;
            Ok(self.rec.borrow().clone())
        }

        fn put(&self, value: Vec<u8>, version: i64) -> Result<(), StoreError> {
            self.gate()?;
            if let Some(f) = self.before_put.borrow_mut().take() {
                f(self);
            }
            let cur = self.rec.borrow().as_ref().map(|r| r.1);
            let next = match (cur, version) {
                (None, 0) => 1,
                (Some(c), v) if v > 0 && c == v => v + 1,
                _ => return Err(StoreError::Conflict),
            };
            self.set(Some((value, next)));
            self.writes.set(self.writes.get() + 1);
            self.answer()
        }

        fn delete(&self, version: i64) -> Result<(), StoreError> {
            self.gate()?;
            match self.rec.borrow().as_ref() {
                Some((_, c)) if *c == version => {}
                _ => return Err(StoreError::Conflict),
            }
            self.set(None);
            self.writes.set(self.writes.get() + 1);
            self.answer()
        }

        fn has_other_keys(&self) -> Result<bool, StoreError> {
            self.gate()?;
            Ok(self.other_keys.get())
        }
    }

    const ME: DeviceId = DeviceId([1; 16]);
    const OTHER: DeviceId = DeviceId([2; 16]);

    #[test]
    fn lock_record_json_round_trips_and_decodes_strictly() {
        let r = LockRecord { device_id: ME, since: 1_700_000_000 };
        let b = r.encode();
        assert_eq!(
            String::from_utf8(b.clone()).unwrap(),
            r#"{"device_id":"01010101010101010101010101010101","since":1700000000}"#
        );
        assert_eq!(LockRecord::decode(&b), Some(r));
        assert_eq!(
            LockRecord::decode(br#"{"device_id":"01010101010101010101010101010101","since":5,"v":2}"#),
            Some(LockRecord { device_id: ME, since: 5 }),
            "later fields are ignored"
        );
        for bad in [
            &b""[..],
            b"null",
            b"[]",
            b"{}",
            br#"{"since":5}"#,
            br#"{"device_id":"0101","since":5}"#,
            br#"{"device_id":"0101010101010101010101010101010G","since":5}"#,
            br#"{"device_id":"01010101010101010101010101010101"}"#,
            br#"{"device_id":"01010101010101010101010101010101","since":-1}"#,
            br#"{"device_id":"01010101010101010101010101010101","since":"5"}"#,
            br#"{"device_id":"0101010101010101010101010101010A","since":5}"#,
        ] {
            assert_eq!(LockRecord::decode(bad), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn acquire_inserts_when_absent_and_reacquires_its_own() {
        let f = Fake::default();
        assert_eq!(acquire(&f, ME, 10), Ok(Held { version: 1, since: 10 }));
        assert_eq!(f.holder(), Some((ME, 1)));
        // Own record (a crash restart): rewritten at the version read.
        assert_eq!(acquire(&f, ME, 20), Ok(Held { version: 2, since: 20 }));
        assert_eq!(f.holder(), Some((ME, 2)));
    }

    #[test]
    fn acquire_refuses_another_device_and_writes_nothing() {
        let f = Fake::holding(OTHER, 4);
        assert_eq!(acquire(&f, ME, 10), Err(LnError::ActiveOnAnotherDevice));
        assert_eq!((f.holder(), f.writes.get()), (Some((OTHER, 4)), 0));
    }

    #[test]
    fn acquire_refuses_a_corrupt_record_and_leaves_it() {
        let f = Fake::default();
        f.set(Some((b"garbage".to_vec(), 3)));
        assert_eq!(acquire(&f, ME, 10), Err(corrupt()));
        assert_eq!(f.writes.get(), 0);
        assert_eq!(*f.rec.borrow(), Some((b"garbage".to_vec(), 3)));
    }

    #[test]
    fn acquire_unreachable_is_backup_unavailable() {
        let f = Fake::default();
        f.down.set(1);
        assert_eq!(acquire(&f, ME, 10), Err(LnError::BackupUnavailable));
        let f = Fake::default();
        f.down.set(0);
        f.lose_response.set(true);
        assert_eq!(acquire(&f, ME, 10), Err(LnError::BackupUnavailable));
        // The write landed: it is ours, so the next start re-acquires.
        assert_eq!(acquire(&f, ME, 11), Ok(Held { version: 2, since: 11 }));
    }

    #[test]
    fn acquire_race_with_another_device_loses() {
        let f = Fake::default();
        *f.before_put.borrow_mut() = Some(Box::new(|f: &Fake| {
            f.set(Some((LockRecord { device_id: OTHER, since: 1 }.encode(), 1)));
        }));
        assert_eq!(acquire(&f, ME, 10), Err(LnError::ActiveOnAnotherDevice));
        assert_eq!(f.holder(), Some((OTHER, 1)));
    }

    #[test]
    fn takeover_writes_at_the_version_read_over_any_holder() {
        let f = Fake::holding(OTHER, 4);
        assert_eq!(takeover(&f, ME, 9, || ()), Ok(Held { version: 5, since: 9 }));
        assert_eq!(f.holder(), Some((ME, 5)));
        // Over a corrupt record (only takeover may) and over none.
        let f = Fake::default();
        f.set(Some((b"{".to_vec(), 2)));
        assert_eq!(takeover(&f, ME, 9, || ()), Ok(Held { version: 3, since: 9 }));
        let f = Fake::default();
        assert_eq!(takeover(&f, ME, 9, || ()), Ok(Held { version: 1, since: 9 }));
    }

    #[test]
    fn concurrent_takeover_or_refresh_makes_the_takeover_lose() {
        let f = Fake::holding(OTHER, 4);
        // Between our read and write, a third device takes over.
        let r = takeover(&f, ME, 9, || {
            let third = DeviceId([3; 16]);
            f.put(LockRecord { device_id: third, since: 1 }.encode(), 4).unwrap();
        });
        assert_eq!(r, Err(LnError::ActiveOnAnotherDevice));
        assert_eq!(f.holder(), Some((DeviceId([3; 16]), 5)));
        let f = Fake::holding(OTHER, 4);
        f.down.set(1);
        assert_eq!(takeover(&f, ME, 9, || ()), Err(LnError::BackupUnavailable));
        assert_eq!(f.holder(), Some((OTHER, 4)));
    }

    #[test]
    fn refresh_bumps_the_held_version() {
        let f = Fake::holding(ME, 3);
        let mut h = Held { version: 3, since: 7 };
        assert_eq!(refresh(&f, ME, &mut h), Ok(Held { version: 4, since: 7 }));
        assert_eq!(h, Held { version: 4, since: 7 });
        assert_eq!(refresh(&f, ME, &mut h), Ok(Held { version: 5, since: 7 }));
    }

    #[test]
    fn refresh_after_a_takeover_or_release_is_lost() {
        let mut held = Held { version: 3, since: 7 };
        let f = Fake::holding(ME, 3);
        takeover(&f, OTHER, 9, || ()).unwrap();
        let e = refresh(&f, ME, &mut held).unwrap_err();
        assert_eq!(e, LnError::ActiveOnAnotherDevice);
        assert!(is_lost(&e));
        assert_eq!(f.holder(), Some((OTHER, 4)), "left alone");
        // Taken over and released again by the other device: gone = lost.
        let f = Fake::default();
        assert_eq!(refresh(&f, ME, &mut held), Err(LnError::ActiveOnAnotherDevice));
        assert_eq!(*f.rec.borrow(), None);
        // Replaced by something unparsable: lost too, and left alone.
        let f = Fake::default();
        f.set(Some((b"?".to_vec(), 9)));
        let e = refresh(&f, ME, &mut held).unwrap_err();
        assert_eq!(e, corrupt());
        assert!(is_lost(&e));
        assert_eq!(*f.rec.borrow(), Some((b"?".to_vec(), 9)));
    }

    #[test]
    fn refresh_adopts_its_own_write_whose_answer_was_lost() {
        let f = Fake::holding(ME, 3);
        f.lose_response.set(true);
        let mut held = Held { version: 3, since: 7 };
        let e = refresh(&f, ME, &mut held).unwrap_err();
        assert_eq!(e, LnError::BackupUnavailable);
        assert!(!is_lost(&e), "unreachable is not lost");
        assert_eq!(f.holder(), Some((ME, 4)), "the write landed");
        assert_eq!(held.version, 3, "response was lost before we knew it landed");
        // Next check at the stale version: conflict, re-read, still ours.
        assert_eq!(refresh(&f, ME, &mut held), Ok(Held { version: 5, since: 7 }));
        assert_eq!(held, Held { version: 5, since: 7 });
        // Even if the rewrite after adopting fails, `held.version` keeps the adopted version.
        let f = Fake::holding(ME, 6);
        let mut held = Held { version: 5, since: 7 };
        f.lose_response.set(true);
        assert_eq!(refresh(&f, ME, &mut held), Err(LnError::BackupUnavailable));
        assert_eq!(held.version, 6, "adopted version retained before rewrite response failed");
    }

    #[test]
    fn refresh_unreachable_is_backup_unavailable_and_writes_nothing() {
        let f = Fake::holding(ME, 3);
        f.down.set(1);
        let mut held = Held { version: 3, since: 7 };
        assert_eq!(refresh(&f, ME, &mut held), Err(LnError::BackupUnavailable));
        assert_eq!(f.holder(), Some((ME, 3)));
        // Conflict, then the re-read fails: could not tell, not lost.
        let f = Fake::holding(ME, 5);
        *f.before_put.borrow_mut() = Some(Box::new(|f: &Fake| f.down.set(1)));
        let mut held = Held { version: 3, since: 7 };
        assert_eq!(refresh(&f, ME, &mut held), Err(LnError::BackupUnavailable));
    }

    #[test]
    fn release_deletes_only_at_the_held_version() {
        let f = Fake::holding(ME, 3);
        assert_eq!(release(&f, Held { version: 3, since: 7 }), Ok(()));
        assert_eq!(*f.rec.borrow(), None);
        // Not ours any more: left as is.
        let f = Fake::holding(OTHER, 4);
        assert_eq!(release(&f, Held { version: 3, since: 7 }), Ok(()));
        assert_eq!(f.holder(), Some((OTHER, 4)));
        // Unreachable: stays ours.
        let f = Fake::holding(ME, 3);
        f.down.set(1);
        assert_eq!(release(&f, Held { version: 3, since: 7 }), Err(LnError::BackupUnavailable));
        assert_eq!(f.holder(), Some((ME, 3)));
    }

    #[test]
    fn state_exists_ignores_the_lock() {
        let f = Fake::holding(ME, 1);
        assert_eq!(state_exists(&f), Ok(false));
        f.other_keys.set(true);
        assert_eq!(state_exists(&f), Ok(true));
        f.down.set(1);
        assert_eq!(state_exists(&f), Err(LnError::BackupUnavailable));
    }

    #[test]
    fn only_conflict_maps_to_conflict() {
        assert_eq!(map_vss_error(&VssError::ConflictError("x".into())), StoreError::Conflict);
        for e in [
            VssError::NoSuchKeyError("x".into()),
            VssError::InvalidRequestError("x".into()),
            VssError::AuthError("x".into()),
            VssError::InternalServerError("x".into()),
        ] {
            assert_eq!(map_vss_error(&e), StoreError::Unavailable, "{e:?}");
        }
    }

    #[test]
    fn device_id_is_created_0600_stable_and_never_replaced() {
        let dir = std::env::temp_dir().join(format!("fortis-device-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = load_or_create_device_id(&dir.join("sub")).unwrap();
        assert_eq!(load_or_create_device_id(&dir.join("sub")).unwrap(), id);
        let path = dir.join("sub").join(DEVICE_ID_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, id.hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{mode:o}");
        }
        assert_ne!(load_or_create_device_id(&dir.join("other")).unwrap(), id);
        std::fs::write(&path, "not an id").unwrap();
        assert_eq!(
            load_or_create_device_id(&dir.join("sub")),
            Err(LnError::Other("device_id_corrupt".into()))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not an id", "left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
