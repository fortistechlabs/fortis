//! RocksDB handles whose open and close never overlap another's.
//!
//! RocksDB's shared environment registers and unregisters each database's
//! paths; opening and closing databases on several threads at once can race
//! there and crash the process (a segfault in `CompositeEnv::UnregisterDbPaths`
//! during a close, seen in the test suite). Every database here is opened and
//! dropped through [`Db`], under one process-wide lock. Production opens each
//! once at start-up, so the lock costs nothing there.

use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard};

use rocksdb::DB;

static OPEN_CLOSE: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    OPEN_CLOSE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A RocksDB database opened (and, when dropped, closed) under the lock.
pub struct Db(ManuallyDrop<DB>);

impl Db {
    /// Run `open` (a `DB::open…` call) under the lock.
    pub fn open<E>(open: impl FnOnce() -> Result<DB, E>) -> Result<Db, E> {
        let _g = lock();
        open().map(|db| Db(ManuallyDrop::new(db)))
    }
}

impl Deref for Db {
    type Target = DB;
    fn deref(&self) -> &DB {
        &self.0
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _g = lock();
        // SAFETY: dropped exactly once, here; `self.0` is never used again.
        unsafe { ManuallyDrop::drop(&mut self.0) }
    }
}
