//! `mushroomdb migrate` on a V11 store (0.10 TY-12): it is already current,
//! and a WAL-only store whose WAL holds a 0.10 record is reported at the
//! version it is actually written at.
use mushroomdb::{GraphDb, RealFs, Value};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mdb-migrate-v11-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_v11_store_is_already_current() {
    let dir = tmp("current");
    let mut db = GraphDb::<RealFs>::open(&dir).unwrap();
    db.insert_node("N", "a", vec![("on".into(), Value::Date(1))])
        .unwrap();
    drop(db);
    assert_eq!(cli::run_migrate(&dir).unwrap(), "already current (V11)\n");
}

#[test]
fn a_wal_only_store_with_a_v11_record_migrates_to_v11() {
    let dir = tmp("wal-only");
    let mut db = GraphDb::<RealFs>::open(&dir).unwrap();
    db.insert_node("N", "a", vec![("on".into(), Value::Date(1))])
        .unwrap();
    drop(db);
    // The upgrade kept the WAL, so without the snapshot the WAL alone holds
    // the store, Date and all.
    std::fs::remove_file(dir.join("snapshot.bin")).unwrap();
    assert_eq!(
        cli::run_migrate(&dir).unwrap(),
        "migrated WAL-only -> V11\n"
    );
    assert_eq!(mushroomdb::snapshot_version_at(&dir).unwrap(), Some(11));
    let reopened = GraphDb::<RealFs>::open(&dir).unwrap();
    assert_eq!(reopened.get_prop("a", "on"), Some(Value::Date(1)));
}
