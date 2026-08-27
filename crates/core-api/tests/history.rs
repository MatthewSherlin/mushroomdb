use core_api::{GraphDb, HistoryChange, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-history-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn node_history_insert_prop_edge_sequence() {
    let dir = tmp("seq");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Person", "a", vec![]).unwrap();
    db.set_prop("a", "color", Value::Str("red".into())).unwrap();
    db.insert_node("Person", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();
    db.remove_prop("a", "color").unwrap();
    db.delete_edge("Knows", "a", "b").unwrap();

    let history_a = db.node_history("a").unwrap();

    // Exactly 5 entries in strict commit order.
    assert_eq!(history_a.len(), 5, "history_a: {history_a:?}");

    // Commits are strictly increasing.
    for w in history_a.windows(2) {
        assert!(
            w[0].commit < w[1].commit,
            "commits not strictly increasing: {:?} >= {:?}",
            w[0].commit,
            w[1].commit
        );
    }

    assert!(
        matches!(&history_a[0].change, HistoryChange::NodeInserted { label } if label == "Person"),
        "expected NodeInserted got {:?}",
        history_a[0]
    );
    assert!(
        matches!(&history_a[1].change, HistoryChange::PropSet { field, value }
            if field == "color" && *value == Value::Str("red".into())),
        "expected PropSet got {:?}",
        history_a[1]
    );
    assert!(
        matches!(&history_a[2].change, HistoryChange::EdgeAdded { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "b" && *outgoing),
        "expected EdgeAdded{{outgoing:true}} got {:?}",
        history_a[2]
    );
    assert!(
        matches!(&history_a[3].change, HistoryChange::PropRemoved { field } if field == "color"),
        "expected PropRemoved got {:?}",
        history_a[3]
    );
    assert!(
        matches!(&history_a[4].change, HistoryChange::EdgeRemoved { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "b" && *outgoing),
        "expected EdgeRemoved{{outgoing:true}} got {:?}",
        history_a[4]
    );

    // history("b") sees NodeInserted + EdgeAdded{outgoing:false} + EdgeRemoved{outgoing:false}
    let history_b = db.node_history("b").unwrap();
    assert_eq!(history_b.len(), 3, "history_b: {history_b:?}");
    assert!(
        matches!(&history_b[0].change, HistoryChange::NodeInserted { label } if label == "Person"),
        "expected NodeInserted got {:?}",
        history_b[0]
    );
    assert!(
        matches!(&history_b[1].change, HistoryChange::EdgeAdded { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "a" && !outgoing),
        "expected EdgeAdded{{outgoing:false}} got {:?}",
        history_b[1]
    );
    assert!(
        matches!(&history_b[2].change, HistoryChange::EdgeRemoved { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "a" && !outgoing),
        "expected EdgeRemoved{{outgoing:false}} got {:?}",
        history_b[2]
    );
}

#[test]
fn node_history_horizon_after_snapshot() {
    let dir = tmp("horizon");
    let mut db = GraphDb::open(&dir).unwrap();

    // Pre-snapshot operations — these should be invisible after a WAL-truncating snapshot.
    db.insert_node("X", "a", vec![]).unwrap();
    db.set_prop("a", "v", Value::Int(1)).unwrap();

    // Default snapshot() truncates the WAL.
    db.snapshot().unwrap();

    // Post-snapshot mutation — the only thing in the new WAL.
    db.set_prop("a", "v", Value::Int(2)).unwrap();

    let history = db.node_history("a").unwrap();

    // Only the post-snapshot PropSet should appear — pre-snapshot history is beyond the horizon.
    assert_eq!(history.len(), 1, "history after snapshot: {history:?}");
    assert!(
        matches!(&history[0].change, HistoryChange::PropSet { field, value }
            if field == "v" && *value == Value::Int(2)),
        "expected post-snapshot PropSet, got {:?}",
        history[0]
    );
}

#[test]
fn node_history_unknown_key_returns_empty() {
    let dir = tmp("empty");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("X", "exists", vec![]).unwrap();
    let history = db.node_history("no_such_key").unwrap();
    assert!(history.is_empty());
}
