use core_api::{Direction, GraphDb, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn snapshot_plus_wal_tail_recovers_everything() {
    let dir = tmp("snap");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("v".into(), Value::Int(1))])
            .unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.insert_edge("E", "a", "b").unwrap();
        db.snapshot().unwrap();
        // post-snapshot writes live only in the wal tail
        db.insert_node("N", "c", vec![]).unwrap();
        db.insert_edge("E", "b", "c").unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 3);
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.get_prop("a", "v"), Some(&Value::Int(1)));
    assert_eq!(db.neighbors("b", "E", Direction::Out).unwrap(), vec!["c"]);
}

#[test]
fn corrupt_snapshot_fails_loudly() {
    let dir = tmp("snapcorrupt");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 0xFF;
    std::fs::write(&path, bytes).unwrap();
    assert!(GraphDb::open(&dir).is_err()); // spec §8: refuse loudly, never guess
}
