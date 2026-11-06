//! `GET /watch` — post-commit mutation events as JSON text frames.
//!
//! After the WebSocket upgrade, the first text frame is always
//! `{"subscribed":true}`. `broadcast::Sender::subscribe` runs in the HTTP
//! handler *before* `on_upgrade` schedules the write task, and the ack is
//! sent before that task calls `recv`. A client that has seen the ack
//! cannot miss subsequent events. Lagged receivers then get a
//! `{"lagged": n}` frame (`n` = skipped events) and continue.

use crate::{AppState, AuthIdentity};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

/// First frame after upgrade. The receiver already exists when this is sent.
const SUBSCRIBED_ACK: &str = r#"{"subscribed":true}"#;

/// Upgrade to a WebSocket and stream one JSON text frame per event.
///
/// `subscribe()` runs here, before `on_upgrade`'s future is scheduled, so
/// the receiver exists before the ack and before any later `recv`. The
/// first frame after upgrade is `{"subscribed":true}`; mutation frames follow.
///
/// Role-bound tokens are denied: the full mutation stream has no mask filter
/// in v1 — denying beats leaking hidden-node events.
pub async fn watch(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "role-bound token: /watch is not permitted"})),
        )
            .into_response();
    }
    if let Some(refusal) = crate::http::refuse_write_tier(&identity) {
        return refusal;
    }
    let rx = state.watch.subscribe();
    ws.on_upgrade(move |socket| write_events(socket, rx))
        .into_response()
}

async fn write_events(mut socket: WebSocket, mut rx: tokio::sync::broadcast::Receiver<String>) {
    if socket
        .send(Message::Text(SUBSCRIBED_ACK.into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(text) = watch_text(event) else {
                    break;
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            // The client sends nothing this endpoint acts on, but the socket
            // has to be read all the same: reading is what answers a ping and
            // a close. Unread, a client's close went unanswered and its task
            // and receiver were held until a later commit's write failed.
            incoming = socket.recv() => {
                // A close frame is not the end: the reply to it is sent by
                // the next read, which then reports the stream finished.
                match incoming {
                    Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Map a broadcast recv result to a JSON text payload. `None` ends the loop.
///
/// Each `Ok(s)` is already a serialised JSON string (produced by the event
/// sink and the edge-prop bridge in `http.rs`). `Lagged` injects a
/// `{"lagged": N}` frame so clients know they may have missed events.
pub(crate) fn watch_text(result: Result<String, RecvError>) -> Option<String> {
    match result {
        Ok(json) => Some(json),
        Err(RecvError::Lagged(n)) => Some(serde_json::json!({"lagged": n}).to_string()),
        Err(RecvError::Closed) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_frame_is_count_object() {
        let text = watch_text(Err(RecvError::Lagged(7))).unwrap();
        assert_eq!(text, r#"{"lagged":7}"#);
    }

    #[test]
    fn closed_ends_the_stream() {
        assert_eq!(watch_text(Err(RecvError::Closed)), None);
    }

    #[test]
    fn event_frame_is_already_serialized_json() {
        // The sink pre-serialises events; watch_text just forwards the string.
        let json = r#"{"node_deleted":{"key":"k"}}"#.to_string();
        let text = watch_text(Ok(json.clone())).unwrap();
        assert_eq!(text, json);
    }

    #[test]
    fn subscribed_ack_is_true_object() {
        assert_eq!(SUBSCRIBED_ACK, r#"{"subscribed":true}"#);
    }

    /// Binding: `MutationEvent::NodeDeleted` serialises to externally-tagged
    /// JSON with a snake_case discriminant.  This pins the wire format that
    /// the event sink pre-serialises and that `watch_text` forwards unchanged.
    #[test]
    fn mutation_event_node_deleted_serializes_to_externally_tagged_json() {
        let ev = mushroomdb::MutationEvent::NodeDeleted { key: "x".into() };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"node_deleted":{"key":"x"}}"#);
        // watch_text must pass the pre-serialised string through as-is.
        let text = watch_text(Ok(json.clone())).unwrap();
        assert_eq!(text, json);
    }
}
