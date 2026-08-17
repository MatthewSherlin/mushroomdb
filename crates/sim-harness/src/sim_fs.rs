use core_storage::fs::{FileId, Fs, FsIntrospect};
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct SimFs {
    files: HashMap<&'static str, Vec<u8>>,
    crash_at: Option<usize>,
    appended: usize,
    crashed: bool,
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

    pub fn with_crash_after(at_bytes: usize) -> Self {
        Self {
            crash_at: Some(at_bytes),
            ..Self::default()
        }
    }

    pub fn total_appended(&self) -> usize {
        self.appended
    }

    pub fn surviving_state(&self) -> SimFs {
        SimFs {
            files: self.files.clone(),
            ..SimFs::default()
        }
    }

    fn check_crash(&mut self) -> std::io::Result<()> {
        if self.crashed {
            return Err(std::io::Error::other("simulated crash"));
        }
        Ok(())
    }
}

impl Fs for SimFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.check_crash()?;
        let budget = self.crash_at.map(|at| at.saturating_sub(self.appended));
        let write_len = match budget {
            Some(b) if b < data.len() => {
                self.crashed = true; // torn write: only a prefix reaches the file
                b
            }
            _ => data.len(),
        };
        self.files
            .entry(name(file))
            .or_default()
            .extend(&data[..write_len]);
        self.appended += write_len;
        if self.crashed {
            return Err(std::io::Error::other("simulated crash mid-append"));
        }
        Ok(())
    }

    fn sync(&mut self, _file: FileId) -> std::io::Result<()> {
        self.check_crash()
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        Ok(self.files.get(name(file)).cloned().unwrap_or_default())
    }

    // Crash injection only fires during append; crashes mid-write_atomic (snapshot
    // write, WAL truncation) are not modeled yet — deliberately deferred to a later
    // plan's DST expansion.
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.check_crash()?;
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
}
