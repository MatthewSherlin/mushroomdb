use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileId {
    Wal,
    Snapshot,
    /// Backup of the previous snapshot, kept until the next clean open at
    /// the current format version. Written before any migration to preserve
    /// the original bytes if the migration step fails.
    SnapshotBak,
    /// RBAC role definitions sidecar. Written atomically by `apply_schema`
    /// when roles change; loaded at open. Never part of WAL/snapshot format.
    Roles,
}

impl FileId {
    fn name(self) -> &'static str {
        match self {
            FileId::Wal => "wal.bin",
            FileId::Snapshot => "snapshot.bin",
            FileId::SnapshotBak => "snapshot.bin.bak",
            FileId::Roles => "roles.json",
        }
    }
}

pub trait Fs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
    fn sync(&mut self, file: FileId) -> std::io::Result<()>;
    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>>;
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
    /// Return the on-disk path of the snapshot file, if any.
    ///
    /// `Some` for `RealFs` (used by `MappedBase::map` for true file mmap).
    /// `None` for `SimFs` and other in-memory implementations (falls back to
    /// `MappedBase::from_bytes`).
    fn snapshot_path(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub trait FsIntrospect {
    fn total_appended(&self) -> usize;
    fn sync_count(&self) -> usize {
        0
    }
}

#[derive(Debug)]
pub struct RealFs {
    dir: PathBuf,
}

impl RealFs {
    pub fn new(dir: &std::path::Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, file: FileId) -> PathBuf {
        self.dir.join(file.name())
    }
}

impl Fs for RealFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(file))?;
        f.write_all(data)
    }

    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        let f = File::open(self.path(file))?;
        full_sync(&f)
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        match File::open(self.path(file)) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok(buf)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        let tmp = self.dir.join(format!("{}.tmp", file.name()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            full_sync(&f)?;
        }
        std::fs::rename(&tmp, self.path(file))?;
        sync_dir(&self.dir)
    }

    fn snapshot_path(&self) -> Option<std::path::PathBuf> {
        Some(self.path(FileId::Snapshot))
    }
}

fn full_sync(file: &File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}

fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("graphdb-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn append_read_and_atomic_write() {
        let mut fs = RealFs::new(&tmp()).unwrap();
        assert_eq!(fs.read(FileId::Wal).unwrap(), Vec::<u8>::new()); // absent = empty
        fs.append(FileId::Wal, b"ab").unwrap();
        fs.append(FileId::Wal, b"cd").unwrap();
        fs.sync(FileId::Wal).unwrap();
        assert_eq!(fs.read(FileId::Wal).unwrap(), b"abcd");
        fs.write_atomic(FileId::Snapshot, b"snap1").unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap2").unwrap(); // replaces
        assert_eq!(fs.read(FileId::Snapshot).unwrap(), b"snap2");
        fs.write_atomic(FileId::Wal, b"").unwrap(); // truncation path
        assert_eq!(fs.read(FileId::Wal).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn write_atomic_replaces_and_still_readable() {
        // existing append_read_and_atomic_write already covers replace;
        // keep it; dir-sync is best-effort observable only via crash tests.
        // Do not fake F_FULLFSYNC in SimFs.
        let d = std::env::temp_dir().join(format!("graphdb-fs-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut fs = RealFs::new(&d).unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap1").unwrap();
        fs.write_atomic(FileId::Snapshot, b"snap2").unwrap();
        assert_eq!(fs.read(FileId::Snapshot).unwrap(), b"snap2");
    }
}
