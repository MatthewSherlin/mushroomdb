#![deny(clippy::await_holding_lock)]

mod access_keys;
mod cors;
mod http;
mod json;
mod mcp;
mod mcp_tasks;
mod subscribe;
mod ws;

use mushroomdb::{AccessKeys, MutationEvent, SharedDb};

pub use http::TxLimits;
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
    /// Full-access token (or no auth configured), or an `admin` key.
    Full,
    /// Write-tier key: mutations allowed; `/backup`, `/stats`, `/metrics`,
    /// `/watch` and `/subscribe` are refused. Reads as full visibility.
    Write,
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

/// Marks a request made with a `read`-tier key. It rides beside
/// [`AuthIdentity::Role`] (the role's mask) and means the request must not
/// write, whatever write scope the role carries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadOnlyKey;

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
    /// The `keys.json` set, when the caller supplied one. Shared, so a reload
    /// is seen by the next request.
    access_keys: Option<AccessKeys>,
    /// Bind address advertised in `GET /health`.
    addr: std::net::SocketAddr,
    /// True when the server is serving over TLS (via the `tls` feature).
    /// When true, the auth cookie gains the `Secure` attribute.
    tls_active: bool,
    /// Instant the router was first built; used by `GET /metrics` uptime_s.
    started_at: std::time::Instant,
    /// `GET /ready` answers 200 while this is true and 503 while it is false.
    /// True at every construction site except the `serve` command, which
    /// starts it false and sets it once the store is open.
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Open interactive transactions (`/tx/*`).
    txs: http::TxStore,
}

pub use access_keys::watch_sighup;
pub use cors::{cors_layer_from_env, parse_cors_origins, CORS_ORIGINS_ENV};

#[allow(deprecated)]
pub use http::{
    router, router_with_access_keys, router_with_auth, router_with_cors, router_with_ready_flag,
    router_with_role_tokens, router_with_tx_limits, router_with_ui, router_with_ui_tls, serve,
    serve_with_role_tokens, serve_with_role_tokens_and_shutdown,
    serve_with_role_tokens_and_shutdown_with_ready_flag, serve_with_shutdown, serve_with_ui,
    serve_with_ui_and_role_tokens, serve_with_ui_and_role_tokens_and_shutdown,
    serve_with_ui_and_role_tokens_and_shutdown_with_ready_flag,
};
#[cfg(feature = "embed-ui")]
pub use http::{
    router_with_embedded_ui, serve_with_embedded_ui, serve_with_embedded_ui_and_shutdown,
    serve_with_embedded_ui_and_shutdown_with_ready_flag,
};
#[cfg(feature = "tls")]
pub use http::{serve_tls, serve_tls_with_shutdown, serve_tls_with_shutdown_with_ready_flag};
