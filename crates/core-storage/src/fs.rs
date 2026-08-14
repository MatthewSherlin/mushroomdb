use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileId {
    Wal,
    Snapshot,
}

impl FileId {
    fn name(self) -> &'static str {
        match self {
            FileId::Wal => "wal.bin",
            FileId::Snapshot => "snapshot.bin",
        }
    }
}

pub trait Fs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
    fn sync(&mut self, file: FileId) -> std::io::Result<()>;
    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>>;
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()>;
}

pub trait FsIntrospect {
    fn total_appended(&self) -> usize;
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
        File::open(self.path(file))?.sync_all()
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
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.path(file))
    }
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
}
