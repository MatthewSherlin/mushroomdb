//! GET /subscribe integration tests.
//!
//! Pattern mirrors watch.rs: real TCP listener via `serve` on port 0,
//! `tokio-tungstenite` client. Client sends a JSON subscribe message and
//! receives `{"subscribed":true}` before events flow.
//!
//! # Lagged WS note
//!
//! A reliable WS-level Lagged test requires the mpsc channel AND the OS socket
//! send buffer to fill before the client reads.  That is racy and depends on
//! OS buffer sizes.  We test the Lagged serialization in subscribe.rs unit
//! tests and the Lagged queue logic in core-api events tests.  The integration
//! test here verifies the subscribe-message + event-payload wire format.

use core_api::{Predicate, RuleDef, SharedDb, Value};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value as Json;
use server::serve;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

fn tmp(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-subscribe-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn spawn_server(db: SharedDb) -> SocketAddr {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        serve(db, "127.0.0.1:0".parse().unwrap(), tx, None)
            .await
            .expect("serve");
    });
    rx.await.expect("ready")
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connect to /subscribe, send the given subscribe JSON, return the stream
/// after receiving the `{"subscribed":true}` ack.
async fn connect_subscribe(addr: SocketAddr, sub_json: &str) -> WsStream {
    let url = format!("ws://{addr}/subscribe");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    ws.send(Message::Text(sub_json.into()))
        .await
        .expect("send subscribe message");
    let ack = next_text(&mut ws).await;
    assert_eq!(
        ack,
        serde_json::json!({"subscribed": true}),
        "first frame must be subscribe ack"
    );
    ws
}

async fn next_text(ws: &mut WsStream) -> Json {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("ws.next timed out after 10s")
            .expect("ws closed")
            .expect("ws err");
        match msg {
            Message::Text(t) => return serde_json::from_str(t.as_str()).expect("json frame"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

fn overlap_rule(name: &str, etype: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: etype.into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    }
}

fn tags(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
}

/// Invariant: connect to /subscribe, write two nodes that fire a rule,
/// receive EdgeFired events with correct JSON payload including commit_seq.
#[tokio::test]
async fn subscribe_ws_receives_edge_fired_with_correct_payload() {
    let db = SharedDb::open(&tmp("ws-fire")).unwrap();
    db.write().create_rule(overlap_rule("rel", "REL")).unwrap();

    let addr = spawn_server(db.clone()).await;
    let mut ws = connect_subscribe(addr, r#"{"rules":["rel"]}"#).await;

    // Two nodes with overlapping tags → rule fires two edges (bidirectional).
    db.write()
        .insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.write()
        .insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();

    // Receive two EdgeFired events (n1→n2 and n2→n1, order may vary).
    let ev1 = next_text(&mut ws).await;
    let ev2 = next_text(&mut ws).await;

    for ev in [&ev1, &ev2] {
        assert_eq!(ev["type"], "edge_fired", "expected edge_fired, got {ev}");
        assert_eq!(ev["rule"], "rel");
        assert_eq!(ev["edge_type"], "REL");
        assert!(ev["commit_seq"].as_u64().unwrap() > 0);
        // Both events must carry a src_key and dst_key.
        assert!(ev["src_key"].is_string());
        assert!(ev["dst_key"].is_string());
    }

    // The two events are for the (n1→n2) and (n2→n1) pairs.
    let mut keys: Vec<(String, String)> = [&ev1, &ev2]
        .iter()
        .map(|e| {
            (
                e["src_key"].as_str().unwrap().to_string(),
                e["dst_key"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            ("n1".to_string(), "n2".to_string()),
            ("n2".to_string(), "n1".to_string())
        ]
    );
}

/// Invariant: EdgeRetracted arrives when a node is deleted.
#[tokio::test]
async fn subscribe_ws_receives_edge_retracted_on_delete() {
    let db = SharedDb::open(&tmp("ws-retract")).unwrap();
    db.write().create_rule(overlap_rule("rel", "REL")).unwrap();
    db.write()
        .insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.write()
        .insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert!(db.read().edge_count() >= 2, "rule must have derived edges");

    let addr = spawn_server(db.clone()).await;
    let mut ws = connect_subscribe(addr, r#"{"rules":["rel"]}"#).await;

    db.write().delete_node("n1").unwrap();

    // Expect at least one EdgeRetracted.
    let ev = next_text(&mut ws).await;
    assert_eq!(
        ev["type"], "edge_retracted",
        "expected edge_retracted, got {ev}"
    );
    assert_eq!(ev["rule"], "rel");
    assert!(ev["commit_seq"].as_u64().unwrap() > 0);
}

/// Invariant: all events in one write_batch share the same commit_seq and
/// arrive as a contiguous run.
#[tokio::test]
async fn subscribe_ws_batch_events_share_commit_seq() {
    let db = SharedDb::open(&tmp("ws-batch")).unwrap();
    db.write().create_rule(overlap_rule("rel", "REL")).unwrap();

    let addr = spawn_server(db.clone()).await;
    let mut ws = connect_subscribe(addr, r#"{"rules":["rel"]}"#).await;

    // One batch: two nodes with overlapping tags → fires in the same commit.
    db.write()
        .write_batch(|b| {
            b.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))]);
            b.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))]);
        })
        .unwrap();

    let ev1 = next_text(&mut ws).await;
    let ev2 = next_text(&mut ws).await;

    let seq1 = ev1["commit_seq"].as_u64().expect("commit_seq must be u64");
    let seq2 = ev2["commit_seq"].as_u64().expect("commit_seq must be u64");
    assert_eq!(seq1, seq2, "same batch → same commit_seq (invariant 5)");
    assert!(seq1 > 0);
}

/// Invariant: subscribe_writes receives write events over the WS.
#[tokio::test]
async fn subscribe_ws_writes_receives_node_events() {
    let db = SharedDb::open(&tmp("ws-writes")).unwrap();
    let addr = spawn_server(db.clone()).await;
    let mut ws = connect_subscribe(addr, r#"{"writes":true}"#).await;

    db.write().insert_node("Person", "alice", vec![]).unwrap();

    let ev = next_text(&mut ws).await;
    assert_eq!(ev["type"], "node_inserted");
    assert_eq!(ev["label"], "Person");
    assert_eq!(ev["key"], "alice");
}

/// Ordering invariant: on a SINGLE live WS connection, a prop SET that fires
/// the rule arrives before a subsequent SET that retracts it, and commit_seq
/// is strictly ascending between the two operations.
///
/// Setup: two nodes with non-overlapping tags.  First SET makes tags overlap
/// (fire).  Second SET breaks the overlap (retract).  All events arrive on one
/// socket and must be in fire-then-retract order with seq_fire < seq_retract.
#[tokio::test]
async fn subscribe_ws_fire_then_retract_ordering_on_single_connection() {
    let db = SharedDb::open(&tmp("ws-ordering")).unwrap();
    db.write().create_rule(overlap_rule("rel", "REL")).unwrap();
    // n1 has tags=["x"], n2 has tags=["y"] — no overlap initially.
    db.write()
        .insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.write()
        .insert_node("A", "n2", vec![("tags".into(), tags(&["y"]))])
        .unwrap();
    assert_eq!(db.read().edge_count(), 0, "no edges before fire");

    let addr = spawn_server(db.clone()).await;
    let mut ws = connect_subscribe(addr, r#"{"rules":["rel"]}"#).await;

    // SET that fires: n2.tags = ["x"] → overlap ["x"]∩["x"] = 1.0 ≥ 0.5
    db.write().set_prop("n2", "tags", tags(&["x"])).unwrap();
    assert!(
        db.read().edge_count() >= 2,
        "edges must exist after fire SET"
    );

    // Drain all EdgeFired events (n1→n2 and n2→n1).
    let ev_fire1 = next_text(&mut ws).await;
    let ev_fire2 = next_text(&mut ws).await;
    for ev in [&ev_fire1, &ev_fire2] {
        assert_eq!(ev["type"], "edge_fired", "expected edge_fired, got {ev}");
    }
    let seq_fire = ev_fire1["commit_seq"].as_u64().expect("commit_seq");
    // Both fire events share the same commit_seq (same commit).
    assert_eq!(ev_fire2["commit_seq"].as_u64().unwrap(), seq_fire);

    // SET that retracts: n2.tags = ["y"] → overlap ["x"]∩["y"] = 0.0 < 0.5
    db.write().set_prop("n2", "tags", tags(&["y"])).unwrap();
    assert_eq!(
        db.read().edge_count(),
        0,
        "edges retracted after second SET"
    );

    // Drain EdgeRetracted events.
    let ev_ret1 = next_text(&mut ws).await;
    let ev_ret2 = next_text(&mut ws).await;
    for ev in [&ev_ret1, &ev_ret2] {
        assert_eq!(
            ev["type"], "edge_retracted",
            "expected edge_retracted, got {ev}"
        );
    }
    let seq_retract = ev_ret1["commit_seq"].as_u64().expect("commit_seq");
    assert_eq!(ev_ret2["commit_seq"].as_u64().unwrap(), seq_retract);

    // Fire events must precede retract events in commit order.
    assert!(
        seq_fire < seq_retract,
        "commit_seq must be strictly ascending: fire={seq_fire} retract={seq_retract}"
    );
}

/// Invariant: unknown rule returns an error frame and closes.
#[tokio::test]
async fn subscribe_ws_unknown_rule_returns_error() {
    let db = SharedDb::open(&tmp("ws-unknown")).unwrap();
    let addr = spawn_server(db.clone()).await;

    let url = format!("ws://{addr}/subscribe");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    ws.send(Message::Text(r#"{"rules":["nonexistent"]}"#.into()))
        .await
        .unwrap();

    let ev = next_text(&mut ws).await;
    assert!(
        ev.get("error").is_some(),
        "expected error frame for unknown rule, got {ev}"
    );
}
