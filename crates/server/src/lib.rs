#![deny(clippy::await_holding_lock)]

mod http;
mod ws;

use core_api::{MutationEvent, SharedDb};

/// Router state: the database plus the watch broadcast fan-out.
#[derive(Clone)]
struct AppState {
    db: SharedDb,
    watch: tokio::sync::broadcast::Sender<MutationEvent>,
}

pub use http::{router, serve};
