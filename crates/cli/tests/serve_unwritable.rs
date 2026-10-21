//! `mushroomdb serve` on a data directory it cannot write fails at start and
//! names the directory, the uid and the fix. Before HO-12b it said "store is
//! busy: write lock held", which pointed an operator at the wrong problem.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// Puts the directory's permissions back and removes it when dropped, so a
/// failed assertion does not leave a 0555 directory behind.
struct Restore<'a>(&'a std::path::Path);

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(self.0);
    }
}

#[test]
fn serve_on_a_read_only_directory_says_it_is_not_writable_not_busy() {
    let dir = std::env::temp_dir().join(format!("serve-unwritable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _restore = Restore(&dir);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root writes through any mode; there is nothing to observe then.
    if std::fs::write(dir.join("probe"), b"x").is_ok() {
        eprintln!("skipped: the test user can write a 0555 directory (root)");
        return;
    }

    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("serve")
        .arg(&dir)
        .args(["--addr", "127.0.0.1:0"])
        .output()
        .expect("run mushroomdb serve");

    assert!(!out.status.success(), "serve must refuse to start");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("busy"),
        "misreported as a held lock: {stderr}"
    );
    assert!(stderr.contains("not writable"), "{stderr}");
    assert!(
        stderr.contains(&dir.display().to_string()),
        "path: {stderr}"
    );
    assert!(stderr.contains("uid"), "uid: {stderr}");
    assert!(stderr.contains("mode 0555"), "owner and mode: {stderr}");
}
