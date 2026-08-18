//! `GET /watch` — post-commit mutation events as JSON text frames.
//!
//! After the WebSocket upgrade, the first text frame is always
//! `{"subscribed":true}`. `broadcast::Sender::subscribe` runs in the HTTP
//! handler *before* `on_upgrade` schedules the write task, and the ack is
//! sent before that task calls `recv`. A client that has seen the ack
//! cannot miss subsequent events. Lagged receivers then get a
//! `{"lagged": n}` frame (`n` = skipped events) and continue.

use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use core_api::MutationEvent;
use tokio::sync::broadcast::error::RecvError;

/// First frame after upgrade. The receiver already exists when this is sent.
const SUBSCRIBED_ACK: &str = r#"{"subscribed":true}"#;

/// Upgrade to a WebSocket and stream one JSON text frame per event.
///
/// `subscribe()` runs here, before `on_upgrade`'s future is scheduled, so
/// the receiver exists before the ack and before any later `recv`. The
/// first frame after upgrade is `{"subscribed":true}`; mutation frames follow.
pub async fn watch(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let rx = state.watch.subscribe();
    ws.on_upgrade(move |socket| write_events(socket, rx))
}

async fn write_events(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<MutationEvent>,
) {
    if socket
        .send(Message::Text(SUBSCRIBED_ACK.into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        let Some(text) = watch_text(rx.recv().await) else {
            break;
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

/// Map a broadcast recv result to a JSON text payload. `None` ends the loop.
pub(crate) fn watch_text(result: Result<MutationEvent, RecvError>) -> Option<String> {
    match result {
        Ok(ev) => Some(serde_json::to_string(&ev).expect("MutationEvent is always serializable")),
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
    fn event_frame_is_externally_tagged_json() {
        let text = watch_text(Ok(MutationEvent::NodeDeleted { key: "k".into() })).unwrap();
        assert_eq!(text, r#"{"node_deleted":{"key":"k"}}"#);
    }

    #[test]
    fn subscribed_ack_is_true_object() {
        assert_eq!(SUBSCRIBED_ACK, r#"{"subscribed":true}"#);
    }
}
