//! A data directory a restore on boot stopped in the middle of (its install
//! marker is there) holds a part of a store. Every open refuses it, naming
//! the marker and the way out: the library, a shared handle and an as-of
//! view, and so every command and binding built on them (HO-10).

use mushroomdb::restore::INSTALL_MARKER;
use mushroomdb::{GraphDb, SharedDb};

#[test]
fn every_open_refuses_a_half_installed_restore() {
    let path = std::env::temp_dir().join(format!(
        "mushroomdb-half-installed-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    let dir = Dir(path);
    {
        let mut db = GraphDb::open(dir.0.as_path()).unwrap();
        db.insert_node("A", "a", vec![]).unwrap();
    }
    std::fs::write(dir.0.as_path().join(INSTALL_MARKER), "snapshot.bin").unwrap();
    let errors = [
        GraphDb::open(dir.0.as_path()).err().map(|e| e.to_string()),
        SharedDb::open(dir.0.as_path()).err().map(|e| e.to_string()),
        GraphDb::open_at(dir.0.as_path(), 0)
            .err()
            .map(|e| e.to_string()),
    ];
    for (what, e) in ["open", "shared open", "open_at"].iter().zip(errors) {
        let e = e.unwrap_or_else(|| panic!("{what} opened a half-installed store"));
        assert!(e.contains(INSTALL_MARKER), "{what}: {e}");
        assert!(e.contains("MUSHROOMDB_RESTORE_FROM_S3"), "{what}: {e}");
    }
    std::fs::remove_file(dir.0.as_path().join(INSTALL_MARKER)).unwrap();
    assert!(GraphDb::open(dir.0.as_path()).unwrap().has_node("a"));
}

struct Dir(std::path::PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
