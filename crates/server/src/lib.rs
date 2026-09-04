#![deny(clippy::await_holding_lock)]

mod http;
mod json;
mod mcp;
mod mcp_tasks;
mod subscribe;
mod ws;

use core_api::{MutationEvent, SharedDb};

pub use mcp::run_mcp_stdio;

/// Resolved authentication identity for a single request.
///
/// Injected into request extensions by `auth_middleware` before any handler
/// runs.  Handlers that need to enforce role-based access control extract it
/// via `Extension<AuthIdentity>`.
#[derive(Clone, Debug)]
pub(crate) enum AuthIdentity {
    /// Full-access token (or no auth configured).
    Full,
    /// Role-bound token; the inner string is the role name.
    Role(String),
}

/// Router state: the database plus the watch broadcast fan-out.
#[derive(Clone)]
struct AppState {
    db: SharedDb,
    watch: tokio::sync::broadcast::Sender<MutationEvent>,
    /// Full-access bearer token (`--token` / `MUSHROOMDB_TOKEN`).
    token: Option<String>,
    /// Role-bound tokens: bearer value → role name.
    /// A non-empty map enables role enforcement on every request.
    role_tokens: std::collections::HashMap<String, String>,
    /// Bind address advertised in `GET /health`.
    addr: std::net::SocketAddr,
    /// True when the server is serving over TLS (via the `tls` feature).
    /// When true, the auth cookie gains the `Secure` attribute.
    tls_active: bool,
    /// Instant the router was first built; used by `GET /metrics` uptime_s.
    started_at: std::time::Instant,
}

#[cfg(feature = "tls")]
pub use http::serve_tls;
#[allow(deprecated)]
pub use http::{
    router, router_with_auth, router_with_role_tokens, router_with_ui, router_with_ui_tls, serve,
    serve_with_role_tokens, serve_with_ui, serve_with_ui_and_role_tokens,
};
#[cfg(feature = "embed-ui")]
pub use http::{router_with_embedded_ui, serve_with_embedded_ui};
