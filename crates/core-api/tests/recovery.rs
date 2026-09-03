use core_api::{Direction, GraphDb, GraphError, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn basic_write_and_read() {
    let dir = tmp("basic");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "u1", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.insert_node("Person", "u2", vec![]).unwrap();
    assert!(matches!(
        db.insert_node("Person", "u1", vec![]),
        Err(GraphError::DuplicateKey { .. })
    ));
    assert!(db.insert_edge("KNOWS", "u1", "u2").unwrap());
    assert!(!db.insert_edge("KNOWS", "u1", "u2").unwrap()); // dup edge
    assert!(matches!(
        db.insert_edge("KNOWS", "u1", "ghost"),
        Err(GraphError::KeyNotFound { .. })
    ));
    db.set_prop("u2", "name", Value::Str("bo".into())).unwrap();
    assert_eq!(db.get_prop("u1", "age"), Some(Value::Int(30)));
    assert_eq!(
        db.neighbors("u1", "KNOWS", Direction::Out).unwrap(),
        vec!["u2"]
    );
    assert_eq!(
        db.neighbors("u2", "KNOWS", Direction::In).unwrap(),
        vec!["u1"]
    );
    assert_eq!(db.node_count(), 2);
    assert_eq!(db.edge_count(), 1);
}

#[test]
fn state_survives_reopen() {
    let dir = tmp("reopen");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "u1", vec![]).unwrap();
        db.insert_node("Person", "u2", vec![("x".into(), Value::Bool(true))])
            .unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
    } // drop = crash without shutdown ceremony
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("u1"));
    assert_eq!(db.get_prop("u2", "x"), Some(Value::Bool(true)));
    assert_eq!(
        db.neighbors("u1", "KNOWS", Direction::Out).unwrap(),
        vec!["u2"]
    );
    assert_eq!(db.edge_count(), 1);
}

#[test]
fn post_crash_garbage_does_not_swallow_subsequent_writes() {
    let dir = tmp("garbage");
    // Session 1: write two nodes and an edge, then crash (drop without ceremony).
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "u1", vec![]).unwrap();
        db.insert_node("Person", "u2", vec![]).unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
    }
    // Simulate crash corruption: append garbage bytes to the WAL file.
    {
        use std::io::Write;
        let wal_path = dir.join("wal.bin");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .unwrap();
        f.write_all(b"\xDE\xAD\xBE\xEF").unwrap();
    }
    // Session 2: open (must recover valid prefix and truncate garbage), write new nodes.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        assert!(db.has_node("u1")); // original data survives
        db.insert_node("Person", "u3", vec![]).unwrap();
        db.insert_node("Person", "u4", vec![]).unwrap();
    }
    // Session 3: open again — u3 and u4 must be present (written after the garbage truncation).
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("u1"));
    assert!(db.has_node("u2"));
    assert!(db.has_node("u3"));
    assert!(db.has_node("u4"));
    assert_eq!(db.edge_count(), 1);
}

/// `repair_wal: false` opens the valid prefix in memory and writes nothing.
///
/// The recall hook opens the user's store on every prompt with no cross-process
/// lock. Truncating a torn tail is correct recovery for a command the user
/// typed; for an unattended reader racing a live `serve` mid-append it would
/// discard a frame the server believes durable. The default (`true`) still
/// truncates — see `post_crash_garbage_does_not_swallow_subsequent_writes`.
#[test]
fn repair_wal_false_leaves_a_torn_wal_on_disk() {
    let dir = tmp("no-wal-repair");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "u1", vec![]).unwrap();
        db.insert_node("Person", "u2", vec![]).unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
    }
    let wal_path = dir.join("wal.bin");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .unwrap();
        f.write_all(b"\xDE\xAD\xBE\xEF").unwrap();
    }
    let torn = std::fs::read(&wal_path).unwrap();

    // Reader open: the valid prefix is replayed, the file is not rewritten.
    {
        let db = GraphDb::open_with_options(
            &dir,
            core_api::OpenOptions {
                auto_migrate: false,
                repair_wal: false,
            },
        )
        .unwrap();
        assert!(db.has_node("u1"));
        assert!(db.has_node("u2"));
        assert_eq!(db.edge_count(), 1);
    }
    assert_eq!(
        std::fs::read(&wal_path).unwrap(),
        torn,
        "repair_wal: false must leave wal.bin byte-identical"
    );

    // The default still repairs.
    drop(GraphDb::open(&dir).unwrap());
    assert_eq!(
        std::fs::read(&wal_path).unwrap().len(),
        torn.len() - 4,
        "the default open truncates the torn tail"
    );
}

/// `OpenOptions::default()` keeps repairing the WAL: the new flag is additive.
#[test]
fn open_options_default_repairs_the_wal() {
    assert!(core_api::OpenOptions::default().repair_wal);
    assert!(core_api::OpenOptions::default().auto_migrate);
}
