//! A data directory the process cannot write to says so. Before HO-12b it
//! answered `Busy` ("write lock held by another process"), which sent an
//! operator hunting for a second process that did not exist.
#![cfg(unix)]

use mushroomdb::{BatchOp, GraphDb, GraphError, SharedDb};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-unwritable-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Make `dir` read-only (mode 0555). Returns `false`, after saying why, when
/// the test user is root: root writes through any mode, so there is nothing to
/// observe.
fn make_read_only(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join("root-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: the test user can write a 0555 directory (root)");
        return false;
    }
    true
}

/// Puts the directory's permissions back and removes it when dropped, so a
/// failed assertion does not leave a 0555 directory behind.
struct Restore<'a>(&'a Path);

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(self.0);
    }
}

fn assert_names_the_cause(err: &GraphError, dir: &Path) {
    assert!(
        matches!(err, GraphError::Unwritable { .. }),
        "expected Unwritable, got {err:?}"
    );
    let text = err.to_string();
    assert!(!text.contains("busy"), "must not claim a held lock: {text}");
    assert!(text.contains("not writable"), "{text}");
    assert!(text.contains(&dir.display().to_string()), "path: {text}");
    assert!(text.contains("uid"), "effective uid: {text}");
    assert!(text.contains("mode 0555"), "owner and mode: {text}");
    assert!(text.contains("Fly") && text.contains("Render"), "{text}");
}

#[test]
fn opening_a_read_only_directory_for_writing_names_the_directory_not_a_lock() {
    let dir = tmp("open");
    let _restore = Restore(&dir);
    if !make_read_only(&dir) {
        return;
    }
    let err = GraphDb::open(&dir).err().expect("open must fail");
    assert_names_the_cause(&err, &dir);
}

#[test]
fn a_shared_handle_on_a_read_only_directory_fails_its_first_write_with_the_cause() {
    let dir = tmp("shared");
    let _restore = Restore(&dir);
    if !make_read_only(&dir) {
        return;
    }
    let db = SharedDb::open(&dir).expect("a shared open takes no lock");
    let err = db
        .write()
        .insert_node("N", "a", vec![])
        .expect_err("the write must fail");
    assert_names_the_cause(&err, &dir);
    // The explicit-wait form reports it up front, too.
    let err = db
        .write_with_wait(std::time::Duration::ZERO)
        .err()
        .expect("write_with_wait must fail");
    assert_names_the_cause(&err, &dir);
}

#[test]
fn a_group_commit_on_a_read_only_directory_fails_with_the_cause_and_the_handle_stays_usable() {
    let dir = tmp("group");
    let _restore = Restore(&dir);
    if !make_read_only(&dir) {
        return;
    }
    let db = SharedDb::open(&dir).expect("a shared open takes no lock");
    let insert = || {
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "a".into(),
            props: vec![],
        }]
    };
    let err = db.submit_batch(insert()).expect_err("the group must fail");
    assert_names_the_cause(&err, &dir);
    // Not degraded: the queue keeps answering with the same cause.
    let again = db.submit_batch(insert()).expect_err("still unwritable");
    assert_names_the_cause(&again, &dir);
}

#[test]
fn a_genuinely_held_lock_is_still_busy() {
    let dir = tmp("busy");
    let _holder = GraphDb::open(&dir).unwrap();
    let err = GraphDb::open(&dir).err().expect("second open must fail");
    assert!(matches!(err, GraphError::Busy { .. }), "got {err:?}");
    drop(_holder);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_failure_that_is_not_a_permission_refusal_is_not_busy_on_any_path() {
    // `LOCK` is a directory, so opening it fails with EISDIR: an I/O fault, not
    // a peer holding the lock and not a permission problem.
    let dir = tmp("lockdir");
    let _restore = Restore(&dir);
    std::fs::create_dir_all(dir.join("LOCK")).unwrap();

    let err = GraphDb::open(&dir).err().expect("open must fail");
    assert!(matches!(err, GraphError::Io(_)), "open: {err:?}");

    let db = SharedDb::open(&dir).expect("a shared open takes no lock");
    let err = db
        .write()
        .insert_node("N", "a", vec![])
        .expect_err("the write must fail");
    assert!(matches!(err, GraphError::Io(_)), "write(): {err:?}");
    let err = db
        .write_with_wait(std::time::Duration::ZERO)
        .err()
        .expect("write_with_wait must fail");
    assert!(matches!(err, GraphError::Io(_)), "write_with_wait: {err:?}");
}
