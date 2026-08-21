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

use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use core_api::{DbEvent, Subscription};
use serde::Deserialize;
use std::time::Duration;
use tokio::task;

/// How long to block in spawn_blocking before yielding back to the async loop.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);

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

    // Stream events.
    loop {
        // Poll all subscriptions in round-robin with a short blocking timeout.
        // spawn_blocking bridges the std blocking recv into async.
        let subs_clone = subs.clone();
        let events: Vec<DbEvent> = task::spawn_blocking(move || {
            let mut out = Vec::new();
            // One pass over all subs, draining non-blocking first.
            for sub in &subs_clone {
                while let Some(ev) = sub.try_recv() {
                    out.push(ev);
                }
            }
            // If nothing arrived, block on the first sub with a timeout.
            if out.is_empty() && !subs_clone.is_empty() {
                if let Some(ev) = subs_clone[0].recv_timeout(RECV_TIMEOUT) {
                    out.push(ev);
                    // Drain remaining from all subs non-blocking.
                    for sub in &subs_clone {
                        while let Some(ev) = sub.try_recv() {
                            out.push(ev);
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default();

        for ev in events {
            let text = serde_json::to_string(&ev).expect("DbEvent is always serializable");
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }

        // Yield to the async runtime between polls.
        tokio::task::yield_now().await;
    }
}
