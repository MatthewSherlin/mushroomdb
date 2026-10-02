#![deny(clippy::await_holding_lock)]

mod http;
mod json;
mod mcp;
mod mcp_tasks;
mod subscribe;
mod ws;

use core_api::{MutationEvent, SharedDb};

pub use mcp::{run_mcp_stdio, run_mcp_stdio_with, ASSOCIATION_TOOLS};

/// The root this crate's `tracing` events are filtered by.
///
/// It is the **lib** name, which is not the package name — `server`, not
/// `mushroomdb-server`. A log filter naming the package matches nothing, and a
/// filter that matches nothing looks exactly like a server with nothing to
/// report. Exported so the binary's default filter can be checked against it
/// rather than against a guess.
pub const LOG_TARGET_ROOT: &str = module_path!();

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
    /// Reached an endpoint that is open by design without presenting a token
    /// that resolves to anything — today only `GET /health`, which a load
    /// balancer must be able to call before it has a credential.
    ///
    /// Distinct from [`Full`](AuthIdentity::Full) because the difference is
    /// what may be disclosed: an anonymous caller gets liveness and nothing
    /// that describes the graph.
    Anonymous,
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

#[allow(deprecated)]
pub use http::{
    router, router_with_auth, router_with_role_tokens, router_with_ui, router_with_ui_tls, serve,
    serve_with_role_tokens, serve_with_role_tokens_and_shutdown, serve_with_shutdown,
    serve_with_ui, serve_with_ui_and_role_tokens, serve_with_ui_and_role_tokens_and_shutdown,
};
#[cfg(feature = "embed-ui")]
pub use http::{
    router_with_embedded_ui, serve_with_embedded_ui, serve_with_embedded_ui_and_shutdown,
};
#[cfg(feature = "tls")]
pub use http::{serve_tls, serve_tls_with_shutdown};
