//! GET /watch integration.
//!
//! Client: `tokio-tungstenite` over a real TCP listener (`serve` on port 0).
//! axum/tower `oneshot` cannot complete a WebSocket upgrade, so a live
//! server is required. The first frame after upgrade is `{"subscribed":true}`
//! (receiver already exists). Tests wait for that ack before writing.

use core_api::{MutationEvent, SharedDb, Value};
use futures_util::StreamExt;
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
        "graphdb-watch-{name}-{}-{nanos}",
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

async fn connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/watch");
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("ws connect");
    let ack = next_text(&mut ws).await;
    assert_eq!(
        ack,
        serde_json::json!({"subscribed": true}),
        "first frame must be the subscribe ack"
    );
    ws
}

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Json {
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

/// Binding: a write through a SharedDb clone is a JSON text frame on /watch.
#[tokio::test]
async fn watch_receives_insert_from_shared_db_clone() {
    let db = SharedDb::open(&tmp("clone-insert")).unwrap();
    let addr = spawn_server(db.clone()).await;
    let mut ws = connect(addr).await;

    db.write()
        .insert_node("A", "k", vec![("n".into(), Value::Str("v".into()))])
        .unwrap();

    let frame = next_text(&mut ws).await;
    let expected = serde_json::to_value(MutationEvent::NodeInserted {
        label: "A".into(),
        key: "k".into(),
    })
    .unwrap();
    assert_eq!(frame, expected);
}

/// Binding: batch inner events then BatchApplied, in order, one frame each.
#[tokio::test]
async fn watch_receives_batch_inner_then_summary() {
    let db = SharedDb::open(&tmp("clone-batch")).unwrap();
    let addr = spawn_server(db.clone()).await;
    let mut ws = connect(addr).await;

    db.write()
        .batch()
        .insert_node("A", "x", vec![])
        .insert_node("A", "y", vec![])
        .insert_edge("E", "x", "y")
        .commit()
        .unwrap();

    let frames: Vec<Json> = vec![
        next_text(&mut ws).await,
        next_text(&mut ws).await,
        next_text(&mut ws).await,
        next_text(&mut ws).await,
    ];
    let expected = vec![
        MutationEvent::NodeInserted {
            label: "A".into(),
            key: "x".into(),
        },
        MutationEvent::NodeInserted {
            label: "A".into(),
            key: "y".into(),
        },
        MutationEvent::EdgeInserted {
            edge_type: "E".into(),
            src: "x".into(),
            dst: "y".into(),
        },
        MutationEvent::BatchApplied { ops: 3 },
    ];
    let expected: Vec<Json> = expected
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(frames, expected);
}

/// Binding: ingest_with_edges is one batch: inner events then Ingested.
#[tokio::test]
async fn watch_ingest_with_edges_emits_inner_then_ingested() {
    use core_api::IngestOptions;
    use std::collections::BTreeMap;

    let db = SharedDb::open(&tmp("ingest-watch")).unwrap();
    db.write().insert_node("Person", "b", vec![]).unwrap();
    let addr = spawn_server(db.clone()).await;
    let mut ws = connect(addr).await;

    let mut row = BTreeMap::new();
    row.insert("id".into(), Value::Str("a".into()));
    db.write()
        .ingest_with_edges(
            "Person",
            vec![row],
            &IngestOptions {
                auto_fk: core_api::AutoFk::Off,
                ..IngestOptions::default()
            },
            &[("KNOWS".into(), "a".into(), "b".into())],
        )
        .unwrap();

    let frames: Vec<Json> = vec![
        next_text(&mut ws).await,
        next_text(&mut ws).await,
        next_text(&mut ws).await,
    ];
    let expected = vec![
        MutationEvent::NodeInserted {
            label: "Person".into(),
            key: "a".into(),
        },
        MutationEvent::EdgeInserted {
            edge_type: "KNOWS".into(),
            src: "a".into(),
            dst: "b".into(),
        },
        MutationEvent::Ingested {
            label: "Person".into(),
            inserted: 1,
        },
    ];
    let expected: Vec<Json> = expected
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(frames, expected);
}

/// Slow-consumer Lagged integration test is omitted: capacity is 1024, and
/// filling the ring depends on the handler *not* draining while the TCP
/// window fills — racy under load, and 1024 durable writes is slow. The
/// `{"lagged": n}` mapping is unit-tested in `server::ws` against
/// `RecvError::Lagged` (deterministic, no socket).
#[tokio::test]
async fn watch_rejected_write_emits_no_frame_then_next_commit_does() {
    let db = SharedDb::open(&tmp("clone-reject")).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let addr = spawn_server(db.clone()).await;
    let mut ws = connect(addr).await;

    assert!(db.write().insert_node("A", "a", vec![]).is_err());
    db.write().insert_node("A", "b", vec![]).unwrap();

    let frame = next_text(&mut ws).await;
    let expected = serde_json::to_value(MutationEvent::NodeInserted {
        label: "A".into(),
        key: "b".into(),
    })
    .unwrap();
    assert_eq!(frame, expected);
}
