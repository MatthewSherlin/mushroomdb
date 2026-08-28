use core_storage::fs::{FileId, Fs, FsIntrospect};
use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

/// In-memory filesystem for deterministic simulation testing.
///
/// Two independent crash modes (at most one active per instance):
///
/// **Byte mode** (`with_crash_after`): fires inside `append` when the cumulative
/// bytes-written counter crosses the threshold.  The torn `append` writes a prefix
/// and sets `byte_crashed`.
/// Failure surface: `append` and `sync` subsequently return `Err`.
/// `read` and `write_atomic` are **not** affected by `byte_crashed`.
///
/// **Op mode** (`with_crash_after_ops`): fires on the *n*-th Fs call (0-indexed,
/// all four methods: append/sync/read/write_atomic).  The failing call leaves no
/// side-effects and sets `op_crashed`.
/// Failure surface: all four methods subsequently return `Err`.
/// A failed `write_atomic` preserves old file content (rename-never-happened); a
/// failed `append` writes nothing (whole-call failure).
///
/// `total_ops()` counts only calls that returned `Ok`.
/// `surviving_state()` clears both crash latches.
#[derive(Debug, Clone, Default)]
pub struct SimFs {
    files: HashMap<&'static str, Vec<u8>>,
    // --- byte-mode fields ---
    crash_at: Option<usize>,
    appended: usize,
    /// Successful `sync` calls (test introspect; not a crash-mode metric).
    syncs: usize,
    /// Set when a torn append fires.  Blocks further `append` and `sync`.
    /// Does NOT affect `read` or `write_atomic`.
    byte_crashed: bool,
    // --- op-mode fields (Cell so `read(&self)` can participate) ---
    crash_after_ops: Option<usize>,
    /// Total Fs calls that returned `Ok` (all four methods).
    ops: Cell<usize>,
    /// Set when the op limit fires.  Blocks all four methods.
    op_crashed: Cell<bool>,
    /// Optional artificial latency injected into each successful `sync` call.
    /// Used by throughput benchmarks to simulate slow-storage (HDD / NVMe with
    /// flush guarantee) without requiring real disk I/O.  Zero by default.
    sync_delay_us: u64,
    // --- WAL archive fields (Task 4) ---
    /// In-memory WAL archives keyed by commit-seq N (wal.<N>.archive).
    archives: HashMap<u64, Vec<u8>>,
    /// Persisted horizon floor: first globally-indexed reachable commit.
    horizon_floor: u64,
    /// Genesis-chain marker (mirrors `wal.genesis` in RealFs).
    /// `true` when the surviving archive chain forms a complete WAL history
    /// from the store's first commit.
    genesis_marker: bool,
    /// Truncation marker (mirrors `wal.truncated` in RealFs).
    /// Set to `true` on the first WAL-truncating snapshot.  Write-once; never
    /// cleared so that cross-session truncation is detectable on reopen.
    truncation_marker: bool,
}

fn name(f: FileId) -> &'static str {
    match f {
        FileId::Wal => "wal",
        FileId::Snapshot => "snapshot",
        FileId::SnapshotBak => "snapshot_bak",
        FileId::Roles => "roles",
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
    /// * Failed `write_atomic` — old file content is preserved.
    /// * Failed `append` — no bytes are written.
    /// * After the crash, every subsequent call also fails.
    pub fn with_crash_after_ops(n_ops: usize) -> Self {
        Self {
            crash_after_ops: Some(n_ops),
            ..Self::default()
        }
    }

    /// Slow-storage simulation: each successful `sync` call sleeps for
    /// `delay_us` microseconds before returning.
    ///
    /// Used by environment-independent throughput benchmarks to prove the
    /// group-commit amortization property without real disk I/O.  A delay of
    /// 5 ms (~5_000 µs) approximates a spinning disk or NVMe with
    /// `F_FULLFSYNC` / `fdatasync` enforcement.
    pub fn with_sync_delay_us(delay_us: u64) -> Self {
        Self {
            sync_delay_us: delay_us,
            ..Self::default()
        }
    }

    /// Total bytes passed to successful `append` calls (byte-mode metric).
    pub fn total_appended(&self) -> usize {
        self.appended
    }

    /// Successful `sync` calls (Relaxed-policy tests).
    pub fn sync_count(&self) -> usize {
        self.syncs
    }

    /// Total Fs calls that returned `Ok` (op-mode metric).
    pub fn total_ops(&self) -> usize {
        self.ops.get()
    }

    /// Return a clean `SimFs` with the same file contents and both crash latches reset.
    pub fn surviving_state(&self) -> SimFs {
        SimFs {
            files: self.files.clone(),
            archives: self.archives.clone(),
            horizon_floor: self.horizon_floor,
            genesis_marker: self.genesis_marker,
            truncation_marker: self.truncation_marker,
            ..SimFs::default() // resets byte_crashed, op_crashed, ops, appended
        }
    }

    /// Check the op-mode crash latch and limit.  Does NOT increment `ops`.
    /// Uses `&self` so that `read(&self)` can participate.
    fn check_op_crash(&self) -> std::io::Result<()> {
        if self.op_crashed.get() {
            return Err(std::io::Error::other("simulated crash (op-mode latch)"));
        }
        if let Some(n) = self.crash_after_ops {
            if self.ops.get() >= n {
                self.op_crashed.set(true);
                return Err(std::io::Error::other("simulated crash (op count)"));
            }
        }
        Ok(())
    }

    /// Increment the op counter.  Called only after a method is confirmed to succeed.
    fn tick_op(&self) {
        self.ops.set(self.ops.get() + 1);
    }
}

impl Fs for SimFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        // Byte-mode latch: blocks further appends once a torn write has occurred.
        if self.byte_crashed {
            return Err(std::io::Error::other("simulated crash (byte-mode latch)"));
        }
        // Op-mode latch: fires before any bytes are written (whole-call failure).
        self.check_op_crash()?;
        // Byte-crash logic: may write a torn prefix.
        let budget = self.crash_at.map(|at| at.saturating_sub(self.appended));
        let write_len = match budget {
            Some(b) if b < data.len() => {
                self.byte_crashed = true; // torn write: only a prefix reaches the file
                b
            }
            _ => data.len(),
        };
        self.files
            .entry(name(file))
            .or_default()
            .extend(&data[..write_len]);
        self.appended += write_len;
        if self.byte_crashed {
            // Torn append failed: do NOT tick ops (call did not succeed).
            return Err(std::io::Error::other("simulated crash mid-append"));
        }
        self.tick_op();
        Ok(())
    }

    fn sync(&mut self, _file: FileId) -> std::io::Result<()> {
        // Byte-mode latch also blocks sync (same failure surface as append).
        if self.byte_crashed {
            return Err(std::io::Error::other("simulated crash (byte-mode latch)"));
        }
        self.check_op_crash()?;
        self.syncs += 1;
        self.tick_op();
        // Artificial latency: simulates slow storage for environment-independent benches.
        if self.sync_delay_us > 0 {
            std::thread::sleep(Duration::from_micros(self.sync_delay_us));
        }
        Ok(())
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        // `read` is NEVER blocked by `byte_crashed` (byte-mode surface is append/sync only).
        // Only op-mode crash applies here.
        self.check_op_crash()?;
        let data = self.files.get(name(file)).cloned().unwrap_or_default();
        self.tick_op();
        Ok(data)
    }

    // Byte-mode: crash injection only fires during `append`; `byte_crashed` does NOT
    // block `write_atomic` (byte-mode failure surface is append/sync only).
    //
    // Op-mode: if `check_op_crash` returns Err, the file is NOT updated — old content
    // stays intact (rename-never-happened semantics, matching RealFs where a crash before
    // fsync+rename leaves the original file untouched).  This closes the Plan 1 gap:
    // DST now injects crashes at snapshot writes and WAL-truncation write_atomics via the
    // op-count sweep.
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        // NOT blocked by `byte_crashed`.
        self.check_op_crash()?;
        self.files.insert(name(file), data.to_vec());
        self.tick_op();
        Ok(())
    }

    // ── WAL archive methods ────────────────────────────────────────────────────

    fn list_archives(&self) -> std::io::Result<Vec<u64>> {
        // Op-mode participates; byte-mode does NOT block list_archives.
        self.check_op_crash()?;
        let mut ns: Vec<u64> = self.archives.keys().copied().collect();
        ns.sort_unstable();
        self.tick_op();
        Ok(ns)
    }

    fn read_archive(&self, n: u64) -> std::io::Result<Vec<u8>> {
        self.check_op_crash()?;
        let data = self.archives.get(&n).cloned().unwrap_or_default();
        self.tick_op();
        Ok(data)
    }

    /// Atomically move the WAL bytes into the archives map (rename semantics:
    /// old WAL disappears, archive appears atomically from the db's perspective).
    /// Op-mode crash fires before any state is changed (whole-call failure).
    fn archive_wal(&mut self, n: u64) -> std::io::Result<()> {
        // NOT blocked by byte_crashed (rename is not an append/sync).
        self.check_op_crash()?;
        let wal = self.files.remove("wal").unwrap_or_default();
        self.archives.insert(n, wal);
        self.tick_op();
        Ok(())
    }

    fn delete_archive(&mut self, n: u64) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.archives.remove(&n);
        self.tick_op();
        Ok(())
    }

    fn read_horizon_floor(&self) -> std::io::Result<u64> {
        self.check_op_crash()?;
        let floor = self.horizon_floor;
        self.tick_op();
        Ok(floor)
    }

    fn write_horizon_floor(&mut self, floor: u64) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.horizon_floor = floor;
        self.tick_op();
        Ok(())
    }

    fn has_genesis_marker(&self) -> bool {
        // Reads are not blocked by op-mode crashes (same as `read`).
        self.genesis_marker
    }

    fn write_genesis_marker(&mut self) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.genesis_marker = true;
        self.tick_op();
        Ok(())
    }

    fn delete_genesis_marker(&mut self) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.genesis_marker = false;
        self.tick_op();
        Ok(())
    }

    fn has_truncation_marker(&self) -> bool {
        // Read — does not go through check_op_crash (consistent with `read` behavior).
        self.truncation_marker
    }

    fn write_truncation_marker(&mut self) -> std::io::Result<()> {
        self.check_op_crash()?;
        self.truncation_marker = true;
        self.tick_op();
        Ok(())
    }
}

impl FsIntrospect for SimFs {
    fn total_appended(&self) -> usize {
        self.appended
    }

    fn sync_count(&self) -> usize {
        self.syncs
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
        assert!(fs.append(FileId::Wal, b"ef").is_err()); // byte latch: dead
        let survivor = fs.surviving_state();
        assert_eq!(survivor.read(FileId::Wal).unwrap(), b"abc"); // torn
    }

    #[test]
    fn byte_crash_does_not_block_read_or_write_atomic() {
        // After a byte-crash, read returns file content and write_atomic still works.
        let mut fs = SimFs::with_crash_after(2);
        fs.append(FileId::Wal, b"ab").unwrap(); // ok: 2 bytes, exactly at threshold
        assert!(fs.append(FileId::Wal, b"cd").is_err()); // tears at 0 more bytes
                                                         // read is NOT blocked by byte_crashed
        assert_eq!(fs.read(FileId::Wal).unwrap(), b"ab");
        // write_atomic is NOT blocked by byte_crashed
        fs.write_atomic(FileId::Snapshot, b"snap").unwrap();
        assert_eq!(fs.read(FileId::Snapshot).unwrap(), b"snap");
        // sync IS blocked by byte_crashed
        assert!(fs.sync(FileId::Wal).is_err());
    }

    #[test]
    fn op_crash_write_atomic_preserves_old_content() {
        // Ops 0-2 succeed; op 3 (second write_atomic) crashes.
        // Old snapshot content must survive — rename-never-happened semantics.
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
    fn op_crash_blocks_read() {
        // After an op-crash, read also fails (op-mode failure surface is all four methods).
        let mut fs = SimFs::with_crash_after_ops(1);
        fs.append(FileId::Wal, b"x").unwrap(); // op 0: ok
        assert!(fs.sync(FileId::Wal).is_err()); // op 1: crashes (op_crashed set)
                                                // read is blocked by op_crashed
        assert!(fs.read(FileId::Wal).is_err());
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

    #[test]
    fn torn_append_does_not_count_as_successful_op() {
        // total_ops() counts only calls that returned Ok.
        // The torn append returns Err — must not be counted.
        let mut fs = SimFs::with_crash_after(3);
        fs.append(FileId::Wal, b"ab").unwrap(); // op 0: 2 bytes ok
        assert_eq!(fs.total_ops(), 1);
        let _ = fs.append(FileId::Wal, b"cd"); // tears after 1 byte, returns Err
        assert_eq!(fs.total_ops(), 1); // still 1 — torn call not counted
    }

    #[test]
    fn successful_sync_increments_sync_count() {
        let mut fs = SimFs::new();
        assert_eq!(fs.sync_count(), 0);
        fs.append(FileId::Wal, b"x").unwrap();
        fs.sync(FileId::Wal).unwrap();
        fs.sync(FileId::Wal).unwrap();
        assert_eq!(fs.sync_count(), 2);
        let mut crashing = SimFs::with_crash_after_ops(0);
        assert!(crashing.sync(FileId::Wal).is_err());
        assert_eq!(crashing.sync_count(), 0);
    }
}
