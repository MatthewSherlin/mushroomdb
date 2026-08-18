//! Concurrent access to a [`GraphDb`] via a process-wide reader-writer lock.
//!
//! This is v1 of the spec's single-writer / epoch-reader model: many concurrent
//! readers **or** one writer. The same `read` / `write` API is the upgrade
//! path — lock-free epoch snapshot readers (Plan 8) replace the `RwLock`
//! without changing callers.

use crate::GraphDb;
use core_storage::RealFs;
use core_storage::Result;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Shared handle to an on-disk [`GraphDb`]. [`Clone`] is cheap and shares state.
///
/// # Event-sink deadlock
///
/// [`GraphDb::set_event_sink`] runs the hook inside `log_then_apply` while
/// this write guard is still held. A sink must never call [`SharedDb::read`]
/// or [`SharedDb::write`] on the same handle (the `RwLock` is not
/// re-entrant). The sink is `Send + Sync`; `std::sync::mpsc::Sender`
/// is not `Sync`. Intended examples: `std::sync::mpsc::SyncSender`,
/// `tokio::sync::mpsc::Sender`, `tokio::sync::broadcast::Sender`, or
/// `Arc<Mutex<Vec<_>>>`.
#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<RwLock<GraphDb<RealFs>>>,
}

const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<SharedDb>;
};

impl SharedDb {
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(RwLock::new(GraphDb::open(dir)?)),
        })
    }

    /// Shared read access. Many readers may hold this concurrently.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn read(&self) -> impl Deref<Target = GraphDb<RealFs>> + '_ {
        // Recover a poisoned lock. A panicked reader cannot corrupt state;
        // this just unblocks the process instead of propagating the poison
        // panic. WAL replay on reopen is the real recovery.
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Exclusive write access. Blocks until no other readers or writers hold
    /// the lock.
    ///
    /// # Deadlock warning
    ///
    /// Do not hold a returned guard while calling any method on the same
    /// [`SharedDb`]; the [`RwLock`] is not re-entrant; doing so deadlocks.
    pub fn write(&self) -> impl DerefMut<Target = GraphDb<RealFs>> + '_ {
        // Recover a poisoned lock. A panicked writer mid-apply can leave
        // partial in-memory state; pre-alpha accepts that. WAL replay on
        // reopen is the real recovery — this just unblocks the process
        // instead of propagating the poison panic.
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }
}
