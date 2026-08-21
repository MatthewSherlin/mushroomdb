//! `GET /subscribe` — post-commit rule and write events as JSON text frames.
//!
//! # Protocol
//!
//! After the WebSocket upgrade the server waits for exactly one JSON
//! subscribe message from the client, then responds with `{"subscribed":true}`
//! before streaming events.
//!
//! **Subscribe message:**
//! ```json
//! {"rules": ["skill_fit", "geo_match"], "writes": true}
//! ```
//! All fields are optional:
//! - `rules`: subscribe to edge-fire / edge-retract events for these rules
//!   (subscribe_rule for each name — unknown rule → `{"error":"..."}` + close).
//! - `writes`: if `true`, also subscribe to node/prop write events.
//! - If both are absent / empty, the client receives no events.
//!
//! **Event frames** are JSON internally-tagged as defined by `DbEvent`:
//! ```json
//! {"type":"edge_fired","rule":"skill_fit","src_key":"p1","dst_key":"proj-1","edge_type":"FIT","commit_seq":1}
//! {"type":"lagged","missed":3}
//! ```
//!
//! **Lagged**: if a subscriber's internal queue overflows, it receives a
//! `{"type":"lagged","missed":N}` frame before continuing. The server drops
//! the connection only on a send error; it does not disconnect slow consumers.
//!
//! # Threading model
//!
//! Each WS connection spawns **one** persistent blocking thread via
//! `tokio::task::spawn_blocking`. That thread loops over the `Subscription`
//! handles, calling `recv_timeout` when idle, and forwards events through a
//! bounded `tokio::sync::mpsc` channel to the async WS writer task.
//!
//! This is O(1) threads per connection — not O(N) blocking tasks per interval
//! as a poll-based design would be.  When the WS is closed the async task
//! drops its `Receiver`.  The bridge thread notices via `tx.is_closed()` at
//! the top of its loop (checked after each `recv_timeout` completes, so within
//! at most one idle timeout — 100 ms), or immediately via `blocking_send` error
//! if an event was in flight at close time.
//!
//! **Multi-subscription idle latency:** when a connection holds subscriptions
//! to multiple rules, the blocking thread round-robins them but blocks only on
//! the first subscription during idle.  Events arriving on secondary
//! subscriptions while the first is quiet may experience up to ~100 ms of
//! additional latency before the bridge wakes and drains them.

use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use core_api::{DbEvent, Subscription};
use serde::Deserialize;
use std::time::Duration;
use tokio::task;

/// How long the bridge thread waits on `recv_timeout` when idle.
const BRIDGE_IDLE_TIMEOUT: Duration = Duration::from_millis(100);

/// Client subscribe message.
#[derive(Debug, Deserialize, Default)]
struct SubscribeMsg {
    #[serde(default)]
    rules: Vec<String>,
    #[serde(default)]
    writes: bool,
}

/// Upgrade to a WebSocket and stream `DbEvent` frames.
pub async fn subscribe(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| run(socket, state))
}

async fn run(mut socket: WebSocket, state: AppState) {
    // Wait for the client's subscribe message.
    let msg = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(t))) => {
                match serde_json::from_str::<SubscribeMsg>(&t) {
                    Ok(m) => break m,
                    Err(e) => {
                        let _ = socket
                            .send(Message::Text(
                                serde_json::json!({"error": format!("bad subscribe message: {e}")})
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                        return;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return,
            // ignore ping/binary/etc
            Some(Ok(_)) => continue,
            Some(Err(_)) => return,
        }
    };

    // Build subscriptions.  Acquire write lock, build all subs, drop the guard
    // before any `.await` so the RwLockWriteGuard is not held across yield points.
    let sub_result: Result<Vec<Subscription>, String> = {
        let mut db = state.db.write();
        let mut subs: Vec<Subscription> = Vec::new();
        let mut err: Option<String> = None;
        for rule in &msg.rules {
            match db.subscribe_rule(rule) {
                Ok(sub) => subs.push(sub),
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        if err.is_none() && msg.writes {
            subs.push(db.subscribe_writes());
        }
        drop(db);
        match err {
            Some(e) => Err(e),
            None => Ok(subs),
        }
    };
    let subs = match sub_result {
        Ok(s) => s,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({"error": e}).to_string().into(),
                ))
                .await;
            return;
        }
    };

    // Ack.
    if socket
        .send(Message::Text(r#"{"subscribed":true}"#.into()))
        .await
        .is_err()
    {
        return;
    }

    // Bridge: one persistent blocking thread forwards events through an mpsc
    // channel to the async WS writer.  When the WS closes, the Receiver drops,
    // the next `blocking_send` fails, and the thread exits cleanly.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<DbEvent>(256);
    let _bridge = task::spawn_blocking(move || bridge_loop(subs, event_tx));

    // Stream events to the WS client.
    loop {
        tokio::select! {
            ev = event_rx.recv() => {
                let Some(ev) = ev else { break; }; // bridge thread exited
                let text = serde_json::to_string(&ev).expect("DbEvent is always serializable");
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                // Break on clean Close, stream end (None), or any socket error
                // (e.g. TCP reset after the client drops the connection).
                match msg {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {} // ping/pong/text from client — ignored
                }
            }
        }
    }
    // Dropping event_rx causes blocking_send in bridge_loop to fail → thread exits.
}

/// Runs in a dedicated blocking thread (one per WS connection).
/// Loops over subscriptions, draining events into `tx`.  Returns when `tx`
/// is closed (WS dropped) or when there are no subscriptions.
fn bridge_loop(subs: Vec<Subscription>, tx: tokio::sync::mpsc::Sender<DbEvent>) {
    if subs.is_empty() {
        return;
    }
    loop {
        // Fast exit when the WS task has dropped the receiver.
        if tx.is_closed() {
            return;
        }
        let mut sent_any = false;
        // Non-blocking drain of all subscriptions.
        for sub in &subs {
            while let Some(ev) = sub.try_recv() {
                if tx.blocking_send(ev).is_err() {
                    return; // receiver dropped — WS closed
                }
                sent_any = true;
            }
        }
        if !sent_any {
            // Nothing queued: block on the first subscription with a timeout,
            // then drain the rest non-blocking before re-looping.
            if let Some(ev) = subs[0].recv_timeout(BRIDGE_IDLE_TIMEOUT) {
                if tx.blocking_send(ev).is_err() {
                    return;
                }
                for sub in &subs {
                    while let Some(ev) = sub.try_recv() {
                        if tx.blocking_send(ev).is_err() {
                            return;
                        }
                    }
                }
            }
            // If recv_timeout returned None (timeout): re-check all subs next iteration.
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_event_serializes_to_type_tagged_json() {
        let ev = DbEvent::Lagged { missed: 5 };
        let text = serde_json::to_string(&ev).unwrap();
        assert_eq!(text, r#"{"type":"lagged","missed":5}"#);
    }

    #[test]
    fn edge_fired_serializes_correctly() {
        let ev = DbEvent::EdgeFired {
            rule: "rel".into(),
            src_key: "n1".into(),
            dst_key: "n2".into(),
            edge_type: "REL".into(),
            weight: Some(0.9),
            commit_seq: 7,
        };
        let j: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(j["type"], "edge_fired");
        assert_eq!(j["rule"], "rel");
        assert_eq!(j["commit_seq"], 7);
        assert_eq!(j["weight"], 0.9);
    }

    #[test]
    fn subscribe_msg_defaults_to_empty() {
        let m: SubscribeMsg = serde_json::from_str("{}").unwrap();
        assert!(m.rules.is_empty());
        assert!(!m.writes);
    }

    #[test]
    fn subscribe_msg_parses_rules_and_writes() {
        let m: SubscribeMsg =
            serde_json::from_str(r#"{"rules":["rel"],"writes":true}"#).unwrap();
        assert_eq!(m.rules, ["rel"]);
        assert!(m.writes);
    }
}
