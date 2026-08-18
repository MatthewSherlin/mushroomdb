#![deny(clippy::await_holding_lock)]

mod http;
mod json;
mod mcp;
mod ws;

use core_api::{MutationEvent, SharedDb};

pub use mcp::run_mcp_stdio;

/// Router state: the database plus the watch broadcast fan-out.
#[derive(Clone)]
struct AppState {
    db: SharedDb,
    watch: tokio::sync::broadcast::Sender<MutationEvent>,
}

pub use http::{router, router_with_ui, serve, serve_with_ui};
