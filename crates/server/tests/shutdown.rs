//! Regression tests for the write-queue shutdown path.
//!
//! In v0.4.1, `WriteQueue::signal_shutdown` stored the shutdown flag and
//! called `notify_all()` WITHOUT holding `pending.lock()`.  The drain thread
//! checks the flag under the mutex and then enters `Condvar::wait` — if the
//! notify fires between the predicate check and the wait, it is lost and
//! `DrainHandle::drop` blocks forever on `join()`.  macOS thread-startup
//! timing enters this window reliably; Linux rarely does.
use core_api::SharedDb;
use std::path::PathBuf;

fn tmp_shutdown(index: usize) -> PathBuf {
    let d =
        std::env::temp_dir().join(format!("graphdb-shutdown-{}-{}", std::process::id(), index,));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Regression: signal_shutdown must not lose its wakeup (missed-wakeup race,
/// v0.4.1 era — drain thread parked forever in Condvar::wait on macOS).
/// 200 open/drop cycles each get a 5s watchdog; a single lost wakeup hangs
/// the drop and trips the watchdog.
#[test]
fn shared_db_drop_never_hangs() {
    for i in 0..200 {
        let path = tmp_shutdown(i);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let db = SharedDb::open(&path).unwrap();
            drop(db);
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("SharedDb drop hung on iteration {i} (missed wakeup)"));
        h.join().unwrap();
    }
}
