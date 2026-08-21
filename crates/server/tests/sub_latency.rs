//! Subscription end-to-end latency measurement.
//!
//! Commit-to-event-received p50/p95 over 1 000 events:
//!   (a) in-process  — Rust `subscribe_writes()`, same thread as writer
//!   (b) WS localhost — `/subscribe?writes=true` via tokio-tungstenite
//!
//! Methodology
//! -----------
//! For each event:
//!   t_pre  = Instant::now()  [before insert_node — commit not yet begun]
//!   insert_node() completes synchronously (WAL fsync + apply + push to queue)
//!   t_post = Instant::now()  [after insert_node — commit done, event in queue]
//!   event received via recv_timeout() / WS frame
//!   t_recv = Instant::now()
//!
//! Reported latency = t_recv - t_post ("post-commit to receive").
//! For in-process this measures queue-pop overhead; for WS it includes
//! bridge-thread wakeup + tokio mpsc + socket round-trip on loopback.
//!
//! Clock: std::time::Instant (monotonic, ~ns resolution on Apple Silicon).

use core_api::{DbEvent, SharedDb};
use futures_util::{SinkExt, StreamExt};
use server::serve;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

const N_EVENTS: usize = 1_000;
const WARMUP: usize = 50;

fn tmp(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("sub-lat-{name}-{}-{nanos}", std::process::id()))
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() as f64 * p / 100.0) as usize).min(v.len() - 1);
    v[idx]
}

fn fmt_us(ns: f64) -> String {
    format!("{:.2} µs", ns / 1_000.0)
}

// ---------------------------------------------------------------------------
// (a) In-process
// ---------------------------------------------------------------------------

#[test]
fn measure_inprocess_subscription_latency() {
    let db = SharedDb::open(&tmp("inproc")).expect("open db");
    let sub = db.write().subscribe_writes().unwrap();

    let mut latencies_ns: Vec<f64> = Vec::with_capacity(N_EVENTS);

    // Warmup: let allocator and cache settle.
    for i in 0..WARMUP {
        let key = format!("w-{i:06}");
        db.write().insert_node("X", &key, vec![]).expect("insert");
        sub.recv_timeout(Duration::from_secs(1))
            .expect("warmup event");
    }

    // Measured run.
    for i in 0..N_EVENTS {
        let key = format!("m-{i:06}");
        // Commit happens synchronously inside insert_node().
        db.write().insert_node("X", &key, vec![]).expect("insert");
        let t_post = Instant::now();
        // Event should already be in queue; recv_timeout returns immediately.
        let ev = sub
            .recv_timeout(Duration::from_secs(1))
            .expect("event within 1s");
        let t_recv = Instant::now();
        // Sanity: right event type.
        assert!(
            matches!(ev, DbEvent::NodeInserted { .. }),
            "expected NodeInserted, got {ev:?}"
        );
        latencies_ns.push((t_recv - t_post).as_nanos() as f64);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p95 = percentile(latencies_ns.clone(), 95.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    // Print so `-- --nocapture` reveals them.
    println!();
    println!("=== In-process subscription latency (N={N_EVENTS}, clock=Instant) ===");
    println!("  p50 = {}", fmt_us(p50));
    println!("  p95 = {}", fmt_us(p95));
    println!("  p99 = {}", fmt_us(p99));
    println!();

    // Non-regressing: both p50 and p95 must be under 1 ms for in-process.
    assert!(
        p50 < 1_000_000.0,
        "p50 in-process latency exceeded 1 ms: {p50} ns"
    );
    assert!(
        p95 < 5_000_000.0,
        "p95 in-process latency exceeded 5 ms: {p95} ns"
    );
}

// ---------------------------------------------------------------------------
// (b) WS on localhost
// ---------------------------------------------------------------------------

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_server(db: SharedDb) -> SocketAddr {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        serve(db, "127.0.0.1:0".parse().unwrap(), tx)
            .await
            .expect("serve");
    });
    rx.await.expect("ready")
}

async fn next_text(ws: &mut WsStream) -> serde_json::Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("WS recv timed out")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Text(t) = msg {
            return serde_json::from_str(t.as_str()).expect("json");
        }
    }
}

#[tokio::test]
async fn measure_ws_subscription_latency() {
    let db = SharedDb::open(&tmp("ws")).expect("open db");
    let addr = spawn_server(db.clone()).await;

    let url = format!("ws://{addr}/subscribe");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    ws.send(Message::Text(r#"{"writes":true}"#.into()))
        .await
        .expect("send subscribe");
    let ack = next_text(&mut ws).await;
    assert_eq!(ack["subscribed"], true, "expected ack, got {ack}");

    let mut latencies_ns: Vec<f64> = Vec::with_capacity(N_EVENTS);

    // Warmup.
    for i in 0..WARMUP {
        let key = format!("w-{i:06}");
        db.write().insert_node("X", &key, vec![]).expect("insert");
        next_text(&mut ws).await; // discard warmup frame
    }

    // Measured run: commit on writer thread, measure WS receive time.
    for i in 0..N_EVENTS {
        let key = format!("m-{i:06}");
        // Commit.
        db.write().insert_node("X", &key, vec![]).expect("insert");
        let t_post = Instant::now();
        // Wait for WS frame.
        let ev = next_text(&mut ws).await;
        let t_recv = Instant::now();
        assert_eq!(
            ev["type"], "node_inserted",
            "expected node_inserted, got {ev}"
        );
        latencies_ns.push((t_recv - t_post).as_nanos() as f64);
    }

    let p50 = percentile(latencies_ns.clone(), 50.0);
    let p95 = percentile(latencies_ns.clone(), 95.0);
    let p99 = percentile(latencies_ns.clone(), 99.0);

    println!();
    println!("=== WS localhost subscription latency (N={N_EVENTS}, clock=Instant) ===");
    println!("  p50 = {}", fmt_us(p50));
    println!("  p95 = {}", fmt_us(p95));
    println!("  p99 = {}", fmt_us(p99));
    println!();

    // Non-regressing: WS latency on loopback < 10 ms p50.
    assert!(
        p50 < 10_000_000.0,
        "p50 WS latency exceeded 10 ms: {p50} ns"
    );
}
