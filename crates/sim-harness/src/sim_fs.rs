use core_storage::fs::{FileId, Fs, FsIntrospect};
use std::cell::Cell;
use std::collections::HashMap;

/// In-memory filesystem for deterministic simulation testing.
///
/// Two independent crash modes (at most one active per instance):
///
/// **Byte mode** (`with_crash_after`): fires inside `append` when the cumulative
/// bytes-written counter crosses the threshold; the torn `append` writes only a
/// prefix (bytes up to the threshold survive), matching real OS behaviour.
///
/// **Op mode** (`with_crash_after_ops`): fires on the *n*-th Fs trait call
/// (0-indexed, all four methods: append/sync/read/write_atomic).  The failing
/// call leaves *no* side-effects: `write_atomic` preserves the old file content
/// (rename-never-happened semantics, matching `RealFs`); `append` writes nothing
/// (whole-call failure rather than a torn prefix — still realistic).
#[derive(Debug, Clone, Default)]
pub struct SimFs {
    files: HashMap<&'static str, Vec<u8>>,
    // --- byte-mode fields ---
    crash_at: Option<usize>,
    appended: usize,
    // --- op-mode fields (Cell because `read` takes &self) ---
    crash_after_ops: Option<usize>,
    /// Total completed Fs calls (all four methods).
    ops: Cell<usize>,
    /// Shared crash latch: once set every subsequent call fails.
    crashed: Cell<bool>,
}

fn name(f: FileId) -> &'static str {
    match f {
        FileId::Wal => "wal",
        FileId::Snapshot => "snapshot",
    }
}

impl SimFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Byte-level crash: crash mid-append once `at_bytes` bytes have been durably written.
    pub fn with_crash_after(at_bytes: usize) -> Self {
        Self {
            crash_at: Some(at_bytes),
            ..Self::default()
        }
    }

    /// Op-level crash: crash on the `n_ops`-th Fs call (0-indexed, all four methods).
    ///
    /// * Failed `write_atomic` — old file content is preserved (rename-never-happened).
    /// * Failed `append` — no bytes are written (whole-call failure).
    /// * After the crash, every subsequent call also fails (latch stays set).
    pub fn with_crash_after_ops(n_ops: usize) -> Self {
        Self {
            crash_after_ops: Some(n_ops),
            ..Self::default()
        }
    }

    /// Total bytes passed to successful `append` calls (byte-mode metric).
    pub fn total_appended(&self) -> usize {
        self.appended
    }

    /// Total number of Fs calls that completed successfully (op-mode metric).
    pub fn total_ops(&self) -> usize {
        self.ops.get()
    }

    /// Return a clean `SimFs` carrying the current file contents but no crash state,
    /// simulating the durable bytes that survived the crash.
    pub fn surviving_state(&self) -> SimFs {
        SimFs {
            files: self.files.clone(),
            ..SimFs::default()
        }
    }

    /// Check the crash latch and, in op-crash mode, trigger a crash when the op
    /// counter reaches the configured limit.  Increments `ops` on success.
    ///
    /// Uses `&self` (via `Cell`) so that `read(&self)` can participate.
    fn check_op_crash(&self) -> std::io::Result<()> {
        if self.crashed.get() {
            return Err(std::io::Error::other("simulated crash"));
        }
        if let Some(n) = self.crash_after_ops {
            if self.ops.get() >= n {
                self.crashed.set(true);
                return Err(std::io::Error::other("simulated crash (op count)"));
            }
        }
        self.ops.set(self.ops.get() + 1);
        Ok(())
    }
}

impl Fs for SimFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        // Op-crash check fires *before* any bytes are written (whole-call failure semantics
        // for op-mode crashes; byte-mode torn-prefix is handled separately below).
        self.check_op_crash()?;
        let budget = self.crash_at.map(|at| at.saturating_sub(self.appended));
        let write_len = match budget {
            Some(b) if b < data.len() => {
                self.crashed.set(true); // torn write: only a prefix reaches the file
                b
            }
            _ => data.len(),
        };
        self.files
            .entry(name(file))
            .or_default()
            .extend(&data[..write_len]);
        self.appended += write_len;
        if self.crashed.get() {
            return Err(std::io::Error::other("simulated crash mid-append"));
        }
        Ok(())
    }

    fn sync(&mut self, _file: FileId) -> std::io::Result<()> {
        self.check_op_crash()
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        self.check_op_crash()?;
        Ok(self.files.get(name(file)).cloned().unwrap_or_default())
    }

    // In byte-crash mode: crash injection fires only during `append` (torn-prefix
    // semantics). `write_atomic` is never reached by the byte-level threshold.
    //
    // In op-crash mode: if `check_op_crash` returns Err, the file is NOT updated —
    // old content stays intact (rename-never-happened semantics, matching the real
    // `RealFs::write_atomic` which atomically replaces via a temp-file rename; a crash
    // before the rename leaves the original file untouched).  This closes the Plan 1
    // gap: DST now injects crashes *at* snapshot writes and WAL-truncation write_atomics
    // via the op-count sweep.
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.files.insert(name(file), data.to_vec());
        Ok(())
    }
}

impl FsIntrospect for SimFs {
    fn total_appended(&self) -> usize {
        self.appended
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_storage::fs::{FileId, Fs};

    #[test]
    fn crash_tears_the_inflight_append() {
        let mut fs = SimFs::with_crash_after(3);
        assert!(fs.append(FileId::Wal, b"ab").is_ok()); // 2 bytes in
        assert!(fs.append(FileId::Wal, b"cd").is_err()); // crashes after 1 more byte
        assert!(fs.append(FileId::Wal, b"ef").is_err()); // dead stays dead
        let survivor = fs.surviving_state();
        assert_eq!(survivor.read(FileId::Wal).unwrap(), b"abc"); // torn
    }

    #[test]
    fn op_crash_write_atomic_preserves_old_content() {
        // Ops 0-2 succeed; op 3 (second write_atomic) crashes.
        // The old snapshot content must survive — rename-never-happened semantics.
        let mut fs = SimFs::with_crash_after_ops(3);
        fs.write_atomic(FileId::Snapshot, b"original").unwrap(); // op 0
        fs.append(FileId::Wal, b"entry").unwrap(); // op 1
        fs.sync(FileId::Wal).unwrap(); // op 2
        assert!(fs.write_atomic(FileId::Snapshot, b"new").is_err()); // op 3: crash
        let survivor = fs.surviving_state();
        assert_eq!(survivor.read(FileId::Snapshot).unwrap(), b"original");
        assert_eq!(survivor.read(FileId::Wal).unwrap(), b"entry");
    }

    #[test]
    fn op_crashed_stays_crashed() {
        // Crash on op 1: op 0 succeeds, op 1 crashes, all subsequent fail.
        let mut fs = SimFs::with_crash_after_ops(1);
        fs.append(FileId::Wal, b"x").unwrap(); // op 0
        assert!(fs.sync(FileId::Wal).is_err()); // op 1: crash
        assert!(fs.append(FileId::Wal, b"y").is_err()); // latch: dead
        assert!(fs.write_atomic(FileId::Wal, b"z").is_err()); // latch: dead
        let survivor = fs.surviving_state();
        assert_eq!(survivor.read(FileId::Wal).unwrap(), b"x");
    }
}
