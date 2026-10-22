//! Thin HTTP wrapper over [`SharedDb`]. Every endpoint is a lock, a public
//! core-api call, then a response — no business logic.
//!
//! # Single-sink design
//!
//! The HTTP router is the designated broadcast producer. [`router`] installs
//! one `broadcast::Sender` as the [`mushroomdb::GraphDb`] event sink. MCP and
//! CLI mutations on the same [`SharedDb`] fire into that same sink (one
//! producer, many `/watch` subscribers). A second [`router`] call replaces
//! the sink and terminates every existing subscriber with
//! [`tokio::sync::broadcast::error::RecvError::Closed`].

use crate::json::{
    edge_history_result_json, namespace_arg, node_edges_json, node_history_json, node_info_json,
    params_from_json, parse_ingest_edges, result_set_json, rule_def_from_json, stamp_namespace,
    stamp_namespace_row,
};
use crate::{AppState, AuthIdentity, ReadOnlyKey};
use arrow_bridge::to_ipc_bytes;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mushroomdb::{
    is_write_query, json_to_rows, json_to_value, AccessKeys, AsOfScope, AutoFk, BackupReport,
    BatchOp, DegreeConfig, Dir, GraphError, IngestOptions, KeyTier, MaskMode, NodeMask,
    PageRankConfig, PropPredicate, ReaderSnapshot, ResultSet, SchemaError, SchemaErrorKind,
    SharedDb, SuggestConfig, Value, WccConfig, SUGGEST_DEFAULT_SEED,
};
use serde_json::{json, Value as Js};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tower_http::services::ServeDir;

// The `/tx/*` routes and the open-transaction store. Declared here, not in
// `lib.rs`, so the module reaches this file's private helpers (`graph_err`,
// `forbidden`, ...) instead of widening each of them to `pub(crate)`.
#[path = "tx.rs"]
mod tx;
pub use tx::TxLimits;
pub(crate) use tx::TxStore;

/// Build the HTTP router over `db`. The graph read endpoints (`/query`,
/// `/node/{key}`, its `/edges` and `/neighborhood`), `/health`, `/metrics` and
/// `/algo/*` run on a lock-free [`ReaderSnapshot`], whose capture never waits
/// behind a writer (see [`SharedDb::reader`]). `/stats`, `/explain`,
/// `/suggest`, `/find_similar` and the history routes read the rule engine or
/// the WAL and take the read lock; `/ingest` takes the write lock. Guards are
/// dropped before any `.await`.
/// `GET /watch` upgrades to a WebSocket fed by the post-commit sink.
///
/// **Call at most once per [`SharedDb`].** A second call replaces the sink
/// and terminates all existing `/watch` subscribers with
/// [`tokio::sync::broadcast::error::RecvError::Closed`].
///
/// Installing the watch sink replaces any previously installed
/// [`mushroomdb::GraphDb::set_event_sink`]. The sink only
/// `broadcast::Sender::send`s (non-blocking) and never re-enters `db`.
///
/// # Blocking
///
/// [`SharedDb`] uses a std [`std::sync::RwLock`]. Write handlers
/// (`query_write`, `/ingest`, `create_rule`) run in
/// `tokio::task::spawn_blocking` and drop the write guard before `.await`.
/// Reads stay on the worker (neighborhood is µs). `suggest` and `algo`
/// already use the blocking pool. A live (non-`as_of`) graph read holds no
/// guard while it executes, so a long read never holds the writer up, and an
/// indexed equality lookup is served from the snapshot's own index.
///
/// Every `router_*` entry point reads `MUSHROOMDB_CORS_ORIGINS` (except
/// [`router_with_cors`], which takes the layer explicitly). Fail-fast on an
/// invalid value holds only for the CLI; a library caller gets CORS off and an
/// error log. Validate with [`crate::cors_layer_from_env`] first to fail early.
pub fn router(db: SharedDb) -> Router {
    router_with_auth(db, None)
}

/// [`router`] with an optional bearer/`?token=` requirement on every route
/// except unauthenticated `GET /health`.
///
/// Role enforcement is not active on this entry point; use
/// [`router_with_role_tokens`] when role-bound tokens are required.
pub fn router_with_auth(db: SharedDb, token: Option<String>) -> Router {
    build_app(
        db,
        token,
        HashMap::new(),
        UiFallback::None,
        default_advertise_addr(),
        false,
    )
}

/// [`router_with_auth`] whose `GET /ready` follows `ready`: 503 while it is
/// false, 200 once it is true. Every other router is ready as soon as it is built.
pub fn router_with_ready_flag(
    db: SharedDb,
    token: Option<String>,
    ready: Arc<AtomicBool>,
) -> Router {
    build_app_with_ready(
        db,
        token,
        HashMap::new(),
        UiFallback::None,
        default_advertise_addr(),
        false,
        ready,
        None,
    )
}

/// [`router_with_role_tokens`] plus the `keys.json` set (see [`AccessKeys`]).
///
/// An `admin` key is full access, a `write` key may mutate but is refused
/// `/backup`, `/stats`, `/metrics`, `/watch` and `/subscribe`, and a `read` key
/// is the role it names. The env token and `role_tokens` keep working beside
/// them. `keys` is shared, so a reload of it (see [`crate::watch_sighup`])
/// reaches every router built from it.
pub fn router_with_access_keys(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    keys: AccessKeys,
) -> Router {
    build_app_with_ready(
        db,
        token,
        role_tokens,
        UiFallback::None,
        default_advertise_addr(),
        false,
        Arc::new(AtomicBool::new(true)),
        Some(keys),
    )
}

/// [`router_with_auth`] with an explicit CORS layer (see [`crate::parse_cors_origins`]).
/// `None` adds no CORS headers. The other routers read `MUSHROOMDB_CORS_ORIGINS`
/// instead; this one ignores the environment.
pub fn router_with_cors(
    db: SharedDb,
    token: Option<String>,
    cors: Option<tower_http::cors::CorsLayer>,
) -> Router {
    build_app_with_cors(
        db,
        token,
        HashMap::new(),
        UiFallback::None,
        default_advertise_addr(),
        false,
        Arc::new(AtomicBool::new(true)),
        cors,
        None,
        TxStore::new(TxLimits::from_env()),
    )
}

/// [`router_with_access_keys`] with the open-transaction limits given instead
/// of read from `MUSHROOMDB_TX_MAX` and `MUSHROOMDB_TX_TIMEOUT_SECS`. For an
/// embedder that serves several routers, and for tests that need a short idle
/// timeout.
pub fn router_with_tx_limits(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    keys: Option<AccessKeys>,
    limits: TxLimits,
) -> Router {
    build_app_with_ready_and_txs(
        db,
        token,
        role_tokens,
        UiFallback::None,
        default_advertise_addr(),
        false,
        Arc::new(AtomicBool::new(true)),
        keys,
        TxStore::new(limits),
    )
}

/// [`router`] with a full-access token and a map of role-bound tokens.
///
/// Role-bound tokens (`role_tokens`: bearer → role name) receive masked reads
/// on `/query` and `/node/*`, and 403 on all write or subscription endpoints.
/// Unknown token: 401.  Token bound to a role not in the DB at request time: 401.
pub fn router_with_role_tokens(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> Router {
    build_app(
        db,
        token,
        role_tokens,
        UiFallback::None,
        default_advertise_addr(),
        false,
    )
}

/// Same as [`router_with_auth`], then `ServeDir` as the fallback so API routes win.
///
/// Role enforcement is not active on this entry point; use
/// [`router_with_role_tokens`] when role-bound tokens are required.
pub fn router_with_ui(
    db: SharedDb,
    ui_dir: impl AsRef<std::path::Path>,
    token: Option<String>,
) -> Router {
    build_app(
        db,
        token,
        HashMap::new(),
        UiFallback::Dir(ui_dir.as_ref().to_path_buf()),
        default_advertise_addr(),
        false,
    )
}

/// Like [`router_with_ui`] but marks the connection as TLS-active so the auth
/// cookie carries the `Secure` attribute. Use when the binary is serving
/// directly over HTTPS (i.e. via `serve_tls`) rather than behind a proxy.
pub fn router_with_ui_tls(
    db: SharedDb,
    ui_dir: impl AsRef<std::path::Path>,
    token: Option<String>,
) -> Router {
    build_app(
        db,
        token,
        HashMap::new(),
        UiFallback::Dir(ui_dir.as_ref().to_path_buf()),
        default_advertise_addr(),
        true,
    )
}

#[cfg(feature = "embed-ui")]
static EMBEDDED_UI: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

/// [`router`] plus the `embed-ui` static tree as fallback.
#[cfg(feature = "embed-ui")]
pub fn router_with_embedded_ui(db: SharedDb) -> Router {
    build_app(
        db,
        None,
        HashMap::new(),
        UiFallback::Embedded,
        default_advertise_addr(),
        false,
    )
}

#[cfg(feature = "embed-ui")]
async fn embedded_fallback(uri: axum::http::Uri) -> Response {
    let rel = if uri.path() == "/" || uri.path().is_empty() {
        "index.html"
    } else {
        uri.path().trim_start_matches('/')
    };
    if rel.split('/').any(|seg| seg == "..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match EMBEDDED_UI.get_file(rel) {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, embedded_ctype(rel))],
            file.contents(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(feature = "embed-ui")]
fn embedded_ctype(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Bind `addr` (port 0 is ephemeral) and serve.
///
/// Sends the resolved local address on `ready` once the listener is accepting.
/// Does not hold a database lock.
///
/// Role enforcement is not active on this entry point; use
/// [`serve_with_role_tokens`] when role-bound tokens are required.
#[deprecated(
    since = "0.2.0",
    note = "Use `serve_with_role_tokens` instead; this variant silently ignores role-token configuration."
)]
#[doc(hidden)]
pub async fn serve(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::None,
        token,
        HashMap::new(),
        None,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve`] with role-bound tokens in addition to the optional full-access token.
///
/// `role_tokens` maps bearer values to role names.  See [`router_with_role_tokens`]
/// for the enforcement semantics.
pub async fn serve_with_role_tokens(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::None,
        token,
        role_tokens,
        None,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve`] plus a UI dist directory mounted behind the API routes.
///
/// Role enforcement is not active on this entry point; use
/// [`serve_with_ui_and_role_tokens`] when role-bound tokens are required.
#[deprecated(
    since = "0.2.0",
    note = "Use `serve_with_ui_and_role_tokens` instead; this variant silently ignores role-token configuration."
)]
#[doc(hidden)]
pub async fn serve_with_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
    token: Option<String>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::Dir(ui_dir),
        token,
        HashMap::new(),
        None,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_with_ui`] with role-bound tokens.
pub async fn serve_with_ui_and_role_tokens(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::Dir(ui_dir),
        token,
        role_tokens,
        None,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve`] plus the compiled-in UI (no-op fallback if `embed-ui` is off).
///
/// `role_tokens` maps bearer values to role names; see [`router_with_role_tokens`]
/// for enforcement semantics.  Pass `HashMap::new()` when no role tokens are needed.
#[cfg(feature = "embed-ui")]
pub async fn serve_with_embedded_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::Embedded,
        token,
        role_tokens,
        None,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// The graceful-shutdown variants, added in 0.6.12.
///
/// Each mirrors the entry point above it and takes one extra argument: a
/// receiver that, when it fires **or is dropped**, stops the server accepting
/// new connections and lets every request already in flight finish. Without
/// one, the only way to stop a server was to abort the task running it, which
/// severs in-flight responses and live `/watch` and `/subscribe` subscribers
/// mid-stream.
///
/// These are additive on purpose. The four originals keep their signatures and
/// their behaviour, so nothing embedding this crate has to change; they simply
/// pass no signal. The cost is eight entry points where four would do, and that
/// was the trade chosen over a breaking parameter.
pub async fn serve_with_shutdown(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::None,
        token,
        HashMap::new(),
        Some(shutdown),
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_with_role_tokens`] that stops gracefully. See [`serve_with_shutdown`].
pub async fn serve_with_role_tokens_and_shutdown(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    serve_with_role_tokens_and_shutdown_with_ready_flag(
        db,
        addr,
        ready,
        token,
        role_tokens,
        shutdown,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_with_role_tokens_and_shutdown`] whose `GET /ready` follows `ready_flag`.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_role_tokens_and_shutdown_with_ready_flag(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready_flag: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::None,
        token,
        role_tokens,
        Some(shutdown),
        ready_flag,
        keys,
    )
    .await
}

/// [`serve_with_ui_and_role_tokens`] that stops gracefully. See [`serve_with_shutdown`].
pub async fn serve_with_ui_and_role_tokens_and_shutdown(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    serve_with_ui_and_role_tokens_and_shutdown_with_ready_flag(
        db,
        addr,
        ready,
        ui_dir,
        token,
        role_tokens,
        shutdown,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_with_ui_and_role_tokens_and_shutdown`] whose `GET /ready` follows `ready_flag`.
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_ui_and_role_tokens_and_shutdown_with_ready_flag(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready_flag: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::Dir(ui_dir),
        token,
        role_tokens,
        Some(shutdown),
        ready_flag,
        keys,
    )
    .await
}

/// [`serve_with_embedded_ui`] that stops gracefully. See [`serve_with_shutdown`].
#[cfg(feature = "embed-ui")]
pub async fn serve_with_embedded_ui_and_shutdown(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    serve_with_embedded_ui_and_shutdown_with_ready_flag(
        db,
        addr,
        ready,
        token,
        role_tokens,
        shutdown,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_with_embedded_ui_and_shutdown`] whose `GET /ready` follows `ready_flag`.
#[cfg(feature = "embed-ui")]
#[allow(clippy::too_many_arguments)]
pub async fn serve_with_embedded_ui_and_shutdown_with_ready_flag(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready_flag: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> std::io::Result<()> {
    serve_inner(
        db,
        addr,
        ready,
        UiFallback::Embedded,
        token,
        role_tokens,
        Some(shutdown),
        ready_flag,
        keys,
    )
    .await
}

enum UiFallback {
    None,
    Dir(PathBuf),
    #[cfg(feature = "embed-ui")]
    Embedded,
}

#[allow(clippy::too_many_arguments)]
async fn serve_inner(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui: UiFallback,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
    ready_flag: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        // Caller dropped the readiness receiver; still serve.
        eprintln!("serve: readiness receiver dropped before bind notify");
    }
    let txs = TxStore::new(TxLimits::from_env());
    let app = build_app_with_ready_and_txs(
        db,
        token,
        role_tokens,
        ui,
        local,
        false,
        ready_flag,
        keys,
        txs.clone(),
    );
    match shutdown {
        None => axum::serve(listener, app).await,
        // Stop accepting, then let every request already in flight finish.
        // Dropping the sender counts as the signal, so a caller that goes away
        // shuts the server down rather than stranding it.
        Some(rx) => {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                    // The start of the drain: open transactions roll back and
                    // a new `begin` is refused.
                    txs.close();
                })
                .await
        }
    }
}

/// Serve over TLS (requires the `tls` cargo feature).
///
/// Accepts a path to a PEM certificate and a PEM private key. Uses rustls
/// via `axum-server`. The `ready` sender fires with the bound local address
/// once the listener is accepting, mirroring the contract of [`serve_inner`].
///
/// Pass `token` / `role_tokens` the same way you would for [`serve_with_role_tokens`].
#[cfg(feature = "tls")]
pub async fn serve_tls(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> std::io::Result<()> {
    let config = crate::tls::http_rustls_config(&cert_path, &key_path)?;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        eprintln!("serve_tls: readiness receiver dropped before bind notify");
    }
    let app = build_app(db, token, role_tokens, UiFallback::None, local, true);
    axum_server::from_tcp_rustls(listener, config)?
        .serve(app.into_make_service())
        .await
}

/// [`serve_tls`] that stops gracefully. See [`serve_with_shutdown`].
///
/// `axum_server` has its own shutdown mechanism — a [`axum_server::Handle`] —
/// rather than axum's `with_graceful_shutdown`, so the signal is bridged onto
/// one here. In-flight requests get [`TLS_GRACE`] to finish before the
/// listener is closed regardless; an unbounded grace period would let one
/// stuck request hold a restart open forever.
#[cfg(feature = "tls")]
#[allow(clippy::too_many_arguments)]
pub async fn serve_tls_with_shutdown(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    serve_tls_with_shutdown_with_ready_flag(
        db,
        addr,
        ready,
        cert_path,
        key_path,
        token,
        role_tokens,
        shutdown,
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await
}

/// [`serve_tls_with_shutdown`] whose `GET /ready` follows `ready_flag`.
#[cfg(feature = "tls")]
#[allow(clippy::too_many_arguments)]
pub async fn serve_tls_with_shutdown_with_ready_flag(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    ready_flag: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> std::io::Result<()> {
    let config = crate::tls::http_rustls_config(&cert_path, &key_path)?;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        eprintln!("serve_tls: readiness receiver dropped before bind notify");
    }
    let txs = TxStore::new(TxLimits::from_env());
    let app = build_app_with_ready_and_txs(
        db,
        token,
        role_tokens,
        UiFallback::None,
        local,
        true,
        ready_flag,
        keys,
        txs.clone(),
    );
    let handle = axum_server::Handle::new();
    tokio::spawn({
        let handle = handle.clone();
        async move {
            let _ = shutdown.await;
            txs.close();
            handle.graceful_shutdown(Some(TLS_GRACE));
        }
    });
    axum_server::from_tcp_rustls(listener, config)?
        .handle(handle)
        .serve(app.into_make_service())
        .await
}

/// How long a TLS server lets in-flight requests finish after a shutdown
/// signal before closing anyway.
#[cfg(feature = "tls")]
const TLS_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

fn default_advertise_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn build_app(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    ui: UiFallback,
    addr: SocketAddr,
    tls_active: bool,
) -> Router {
    build_app_with_ready(
        db,
        token,
        role_tokens,
        ui,
        addr,
        tls_active,
        Arc::new(AtomicBool::new(true)),
        None,
    )
}

/// Reads `MUSHROOMDB_CORS_ORIGINS`. An invalid value does not fail here: the CLI
/// validates before opening the store; other callers get CORS off plus an error log.
#[allow(clippy::too_many_arguments)]
fn build_app_with_ready(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    ui: UiFallback,
    addr: SocketAddr,
    tls_active: bool,
    ready: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
) -> Router {
    build_app_with_ready_and_txs(
        db,
        token,
        role_tokens,
        ui,
        addr,
        tls_active,
        ready,
        keys,
        TxStore::new(TxLimits::from_env()),
    )
}

/// [`build_app_with_ready`] over a transaction store the caller holds, so the
/// serve loop can close it when the shutdown signal arrives.
#[allow(clippy::too_many_arguments)]
fn build_app_with_ready_and_txs(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    ui: UiFallback,
    addr: SocketAddr,
    tls_active: bool,
    ready: Arc<AtomicBool>,
    keys: Option<AccessKeys>,
    txs: TxStore,
) -> Router {
    // The CLI validates the variable before it opens the store; an invalid value
    // reaching here (library use) fails closed: no CORS, and an error logged.
    let cors = crate::cors::cors_layer_from_env().unwrap_or_else(|e| {
        tracing::error!("{e}; CORS disabled");
        None
    });
    build_app_with_cors(
        db,
        token,
        role_tokens,
        ui,
        addr,
        tls_active,
        ready,
        cors,
        keys,
        txs,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_app_with_cors(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    ui: UiFallback,
    addr: SocketAddr,
    tls_active: bool,
    ready: Arc<AtomicBool>,
    cors: Option<tower_http::cors::CorsLayer>,
    keys: Option<AccessKeys>,
    txs: TxStore,
) -> Router {
    debug_assert!(
        !db.read().has_event_sink(),
        "router() must be called at most once per SharedDb; a second call \
         replaces the sink and terminates all existing /watch subscribers \
         with RecvError::Closed"
    );
    let (tx, _) = tokio::sync::broadcast::channel(1024);
    {
        let tx = tx.clone();
        db.write().set_event_sink(Box::new(move |ev| {
            let _ = tx.send(ev);
        }));
    }
    let state = AppState {
        db,
        watch: tx,
        token,
        role_tokens,
        addr,
        tls_active,
        started_at: std::time::Instant::now(),
        ready,
        access_keys: keys,
        txs,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready_handler))
        .route("/query", post(query))
        .route("/schema/indexes", get(schema_indexes_list))
        .route("/schema/indexes", post(schema_index_create))
        .route(
            "/schema/indexes/{name}",
            axum::routing::delete(schema_index_drop),
        )
        .route("/schema/constraints", get(schema_constraints_list))
        .route("/schema/constraints", post(schema_constraint_create))
        .route(
            "/schema/constraints/{name}",
            axum::routing::delete(schema_constraint_drop),
        )
        .route("/stats", get(stats))
        .route("/metrics", get(metrics))
        .route("/ingest", post(ingest))
        .route("/rules", post(create_rule))
        .route("/suggest", get(suggest))
        .route("/explain", get(explain))
        .route("/find_similar", post(find_similar))
        .route("/node/{key}", get(node_info))
        .route("/node/{key}", axum::routing::delete(delete_node))
        .route("/node/{key}/edges", get(node_edges))
        .route("/node/{key}/neighborhood", get(neighborhood))
        .route("/node/{key}/history", get(node_history_handler))
        .route("/history/edge", get(edge_history_handler))
        .route("/history/was_linked", get(was_linked_handler))
        .route(
            "/node/{key}/prop/{field}",
            axum::routing::put(set_node_prop),
        )
        .route(
            "/node/{key}/prop/{field}",
            axum::routing::delete(remove_node_prop),
        )
        // Simple BatchOp-mapped endpoints — routed through the group-commit
        // queue (submit_batch) so concurrent node/edge CRUD does not hold
        // the write lock during fsync.
        .route("/nodes", post(create_node))
        .route("/nodes/{key}/rename", post(rename_node))
        .route("/edges", post(create_edge))
        .route("/edges/upsert", post(upsert_edge))
        .route(
            "/edges/{etype}/{src}/{dst}",
            axum::routing::delete(delete_edge),
        )
        .route("/algo/pagerank", post(algo_pagerank))
        .route("/algo/wcc", post(algo_wcc))
        .route("/algo/degree", post(algo_degree))
        .route("/backup", post(backup))
        .merge(tx::routes())
        .with_state(state.clone());

    // The two WebSocket routes are built separately and merged unlayered: a
    // request timeout applied to them would sever a healthy subscription at
    // the deadline, and a subscription is meant to outlive any request.
    let streaming = Router::new()
        .route("/watch", get(crate::ws::watch))
        .route("/subscribe", get(crate::subscribe::subscribe))
        .with_state(state.clone());

    // Everything else gets a deadline and a ceiling.
    //
    // `RequestBodyTimeoutLayer` is deliberately not what is wanted here: the
    // risk is a handler that never finishes, not only a client that never
    // finishes sending. `TimeoutLayer` bounds the whole exchange and answers
    // 408 when it expires.
    //
    // The concurrency limit sheds rather than queues. An unbounded queue in
    // front of a single-writer engine converts overload into latency that
    // never recovers; 503 is the honest answer, and the one a load balancer
    // can act on.
    let app = app
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(
            tower::ServiceBuilder::new()
                // Load shedding surfaces as a `BoxError`, and a Router's stack
                // must be infallible, so the overload is mapped to a response
                // here rather than escaping as an error.
                .layer(axum::error_handling::HandleErrorLayer::new(
                    |_: tower::BoxError| async {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": "server at its concurrency limit; retry"
                            })),
                        )
                    },
                ))
                .load_shed()
                .concurrency_limit(MAX_CONCURRENT_REQUESTS),
        );
    let app = app.merge(streaming);
    let app = match ui {
        UiFallback::None => app,
        UiFallback::Dir(dir) => app.fallback_service(ServeDir::new(dir)),
        #[cfg(feature = "embed-ui")]
        UiFallback::Embedded => app.fallback(embedded_fallback),
    };
    let app = app
        .layer(middleware::from_fn_with_state(state, auth_middleware))
        // Outside auth, so a 401 is logged too — an unauthenticated flood is
        // exactly the thing an operator needs to see.
        .layer(middleware::from_fn(log_middleware))
        // Cap request bodies (default axum limit is only 2 MiB; we allow larger
        // ingest batches but reject multi-GB bodies that would OOM the collector
        // before any handler runs).
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES));
    // Outermost: a preflight is answered here, before auth (it carries no
    // credentials), and a 401 still gets CORS headers so a page can read it.
    match cors {
        Some(cors) => app
            .layer(cors)
            .layer(middleware::from_fn(crate::cors::strip_unless_allowed)),
        None => app,
    }
}

/// Maximum accepted HTTP request body size (64 MiB). Large enough for batched
/// `/ingest` payloads, small enough to prevent a single request from
/// exhausting memory. Clients with larger imports should chunk.
/// Per-request logging, with a policy chosen so the log stays readable.
///
/// A line per request at one level is what makes a server log unreadable: the
/// 200s bury the 500s, and nobody reads it after the first week. So the level
/// follows what happened:
///
/// | Outcome | Level | Visible by default |
/// |---|---|---|
/// | 5xx | `error` | yes — always worth a look |
/// | slower than [`SLOW_REQUEST`] | `warn` | yes — the request that will time out next |
/// | 4xx | `info` | yes — one line, the caller's mistake or a probe |
/// | everything else | `debug` | no |
///
/// A healthy server is therefore quiet, and turning `debug` on for one module
/// gives the full trace without recompiling.
///
/// Every response carries `x-request-id`, echoed from the caller's header when
/// it sent one and generated otherwise, so an error someone reports can be
/// found in the log. Before 0.6.12 there was no `tracing`, no access log and no
/// request id anywhere in the tree: a 500 in production could not be tied to
/// the request that caused it.
async fn log_middleware(req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.is_ascii())
        .map(str::to_owned)
        .unwrap_or_else(new_request_id);

    let method = req.method().clone();
    // The path only. A query string can carry `?token=`, and a log is exactly
    // the wrong place for a credential.
    let path = req.uri().path().to_owned();
    let started = std::time::Instant::now();

    let mut res = next.run(req).await;
    let status = res.status();
    let ms = started.elapsed().as_millis();

    if status.is_server_error() {
        tracing::error!(%request_id, %method, %path, status = status.as_u16(), ms, "request failed");
    } else if started.elapsed() >= SLOW_REQUEST {
        tracing::warn!(%request_id, %method, %path, status = status.as_u16(), ms, "slow request");
    } else if status.is_client_error() {
        tracing::info!(%request_id, %method, %path, status = status.as_u16(), ms, "request refused");
    } else {
        tracing::debug!(%request_id, %method, %path, status = status.as_u16(), ms, "request");
    }

    if let Ok(v) = axum::http::HeaderValue::from_str(&request_id) {
        res.headers_mut().insert("x-request-id", v);
    }
    res
}

/// A request id with no dependency on a uuid crate: the process id, a
/// monotonic counter, and the low bits of a nanosecond clock. Unique enough to
/// correlate a report with a log line, which is all it is for.
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}-{:x}-{:x}", std::process::id(), n, nanos)
}

/// A request slower than this is logged at `warn` whatever its status. Well
/// under [`REQUEST_TIMEOUT`], so the log names the requests heading for a
/// timeout before one happens.
const SLOW_REQUEST: std::time::Duration = std::time::Duration::from_secs(5);

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// How long any one non-streaming request may take before the server answers
/// `408 Request Timeout` and drops it.
///
/// Sized for the slowest legitimate handler rather than the typical one: a
/// `/query` against a large store, a `/backup`, or a `/suggest` that profiles
/// the graph can all take tens of seconds. The point is not to make slow
/// requests fast — it is that a handler which will never finish stops holding
/// a connection, a permit and a read guard forever.
///
/// The WebSocket routes are exempt; see `build_app`.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How many non-streaming requests may be in flight at once before the server
/// sheds load with `503 Service Unavailable`.
///
/// The engine is single-writer and many-reader, so the ceiling exists to bound
/// memory and the blocking pool, not to serialise work. Chosen well above any
/// plausible operator workload and well below what would exhaust the process.
const MAX_CONCURRENT_REQUESTS: usize = 256;

/// Readiness for a container platform. Needs no token and says nothing about
/// the graph.
async fn ready_handler(State(state): State<AppState>) -> Response {
    if state.ready.load(Ordering::Acquire) {
        json_ok(json!({ "ok": true }))
    } else {
        let mut response = json_ok(json!({ "ok": false, "reason": "not ready" }));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        response
    }
}

async fn health(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    // Liveness is for everyone; the graph's shape is not. `/stats` and
    // `/metrics` already refuse a role-bound token because counters leak graph
    // size, and an endpoint that hands the same counters to a caller with no
    // token at all makes that refusal decorative.
    if !matches!(identity, AuthIdentity::Full) {
        return json_ok(json!({ "ok": true }));
    }
    let (nodes, _, edges) = state.db.reader().counts();
    json_ok(json!({
        "ok": true,
        "nodes": nodes,
        "edges": edges,
        "addr": state.addr.to_string(),
    }))
}

/// Run a GraphDb write on the blocking pool. The write guard lives only
/// inside `f` and is dropped before this future awaits.
async fn blocking_write<T, F>(f: F) -> std::result::Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce() -> mushroomdb::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(graph_err(e)),
        Err(_) => Err(err_response("write task panicked")),
    }
}

const TOKEN_COOKIE: &str = "mushroomdb_token";

async fn auth_middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    // No auth configured at all: every request is Full, no restriction.
    let keys_enforce = state
        .access_keys
        .as_ref()
        .is_some_and(AccessKeys::enforcing);
    if state.token.is_none() && state.role_tokens.is_empty() && !keys_enforce {
        req.extensions_mut().insert(AuthIdentity::Full);
        return next.run(req).await;
    }

    let presented = request_token(&req);

    // Health stays reachable without a credential — a load balancer has none —
    // but reaching it is not the same as being trusted by it. A caller whose
    // token does not resolve to full access is `Anonymous` here, and the
    // handler discloses liveness only.
    //
    // Before 0.6.12 this inserted `Full` unconditionally, so an unauthenticated
    // caller read `nodes_live` and `edges` from `/health` while a role-bound
    // token was refused the same counters by `/metrics` and `/stats` on the
    // stated grounds that counters leak graph size. Two endpoints, one
    // disclosure, opposite policies.
    if req.method() == Method::GET && req.uri().path() == "/ready" {
        req.extensions_mut().insert(AuthIdentity::Anonymous);
        return next.run(req).await;
    }
    if req.method() == Method::GET && req.uri().path() == "/health" {
        let full = state
            .token
            .clone()
            .filter(|s| !s.is_empty())
            .zip(presented.as_deref())
            .is_some_and(|(tok, p)| constant_time_eq(p.as_bytes(), tok.as_bytes()))
            || presented
                .as_deref()
                .zip(state.access_keys.as_ref())
                .is_some_and(|(p, keys)| keys.resolve(p) == Some(KeyTier::Admin));
        req.extensions_mut().insert(if full {
            AuthIdentity::Full
        } else {
            AuthIdentity::Anonymous
        });
        return next.run(req).await;
    }

    // Check full-access token first.
    if let Some(ref full_tok) = state.token.clone().filter(|s| !s.is_empty()) {
        if presented
            .as_deref()
            .is_some_and(|p| constant_time_eq(p.as_bytes(), full_tok.as_bytes()))
        {
            let set_cookie = presented_bearer_or_query(&req).as_deref() == Some(full_tok.as_str());
            req.extensions_mut().insert(AuthIdentity::Full);
            let mut res = next.run(req).await;
            if set_cookie && is_html_response(&res) {
                attach_token_cookie(&mut res, full_tok, state.tls_active);
            }
            return res;
        }
    }

    // Check role-bound tokens.
    if let Some(tok) = presented.as_deref() {
        if let Some(role_name) = role_for_token(&state.role_tokens, tok) {
            // Early-deny paths whose handlers use WebSocket upgrade: the WS
            // extraction consumes the request before the handler body runs, so
            // the handler-level identity check is unreachable in those cases.
            // Returning 403 here also removes the need for the handler to
            // hold a read lock just to enforce this.
            let path = req.uri().path();
            if path == "/subscribe" || path == "/watch" {
                return forbidden("role-bound token: this endpoint is not permitted");
            }
            req.extensions_mut()
                .insert(AuthIdentity::Role(role_name.clone()));
            return next.run(req).await;
        }
    }

    // Check the keys.json set. `resolve` compares every key in constant time.
    if let Some(tier) = presented
        .as_deref()
        .zip(state.access_keys.as_ref())
        .and_then(|(tok, keys)| keys.resolve(tok))
    {
        let path = req.uri().path();
        let ws_path = path == "/subscribe" || path == "/watch";
        match tier {
            KeyTier::Admin => {
                let set_cookie = presented_bearer_or_query(&req);
                req.extensions_mut().insert(AuthIdentity::Full);
                let mut res = next.run(req).await;
                if let Some(token) = set_cookie.filter(|_| is_html_response(&res)) {
                    attach_token_cookie(&mut res, &token, state.tls_active);
                }
                return res;
            }
            KeyTier::Write => {
                // The WS extractor runs before the handler body, so these
                // are refused here as well as in the handlers.
                if ws_path {
                    return forbidden(WRITE_TIER_REFUSAL);
                }
                req.extensions_mut().insert(AuthIdentity::Write);
                return next.run(req).await;
            }
            KeyTier::Read(role_name) => {
                if ws_path {
                    return forbidden("role-bound token: this endpoint is not permitted");
                }
                // A role may carry a write scope; a read key never writes.
                if !read_key_may(req.method(), path) {
                    return forbidden(READ_TIER_REFUSAL);
                }
                req.extensions_mut().insert(AuthIdentity::Role(role_name));
                req.extensions_mut().insert(ReadOnlyKey);
                return next.run(req).await;
            }
        }
    }

    // Nothing matched → 401.
    unauthorized()
}

/// The role bound to `presented`, comparing it with every role token in
/// constant time (a hash-map lookup would stop at the first difference).
fn role_for_token<'a>(
    role_tokens: &'a HashMap<String, String>,
    presented: &str,
) -> Option<&'a String> {
    let mut found = None;
    for (token, role) in role_tokens {
        if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
            found = Some(role);
        }
    }
    found
}

/// Whether a read-tier key may make this request at all. An allowlist, so a
/// route added later is refused to read keys until someone decides otherwise:
/// reads (`GET`), the two `POST` routes that carry a read (`/query`, whose
/// handler refuses a write statement, and `/find_similar`), and the `/tx/*`
/// routes: a read key may hold a transaction, whose handler refuses a write.
fn read_key_may(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET | Method::HEAD => true,
        Method::POST => path == "/query" || path == "/find_similar" || path.starts_with("/tx/"),
        Method::DELETE => path.starts_with("/tx/"),
        _ => false,
    }
}

/// The 403 detail a read-tier key gets on a write.
pub(crate) const READ_TIER_REFUSAL: &str = "read-tier key: writes are not permitted";

fn request_token(req: &Request) -> Option<String> {
    presented_bearer_or_query(req).or_else(|| presented_cookie(req))
}

fn presented_bearer_or_query(req: &Request) -> Option<String> {
    if let Some(header) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(value) = bearer_token(header) {
            return Some(value.to_string());
        }
    }
    query_param(req.uri().query().unwrap_or(""), "token")
}

fn presented_cookie(req: &Request) -> Option<String> {
    let header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    cookie_named(header, TOKEN_COOKIE).map(str::to_string)
}

fn cookie_named<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';') {
        let part = part.trim();
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim() == name {
            return Some(v.trim());
        }
    }
    None
}

fn is_html_response(res: &Response) -> bool {
    res.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/html")
        })
}

fn attach_token_cookie(res: &mut Response, token: &str, secure: bool) {
    let secure_attr = if secure { "; Secure" } else { "" };
    let value = format!("{TOKEN_COOKIE}={token}; Path=/; SameSite=Lax; HttpOnly{secure_attr}");
    if let Ok(hv) = HeaderValue::from_str(&value) {
        res.headers_mut().insert(header::SET_COOKIE, hv);
    }
}

fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, value) = header.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") {
        Some(value.trim())
    } else {
        None
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) if k == key => return percent_decode_plus(v),
            None if pair == key => return Some(String::new()),
            _ => {}
        }
    }
    None
}

/// `application/x-www-form-urlencoded`: `+` is space, `%HH` is a byte.
fn percent_decode_plus(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = from_hex(bytes[i + 1])?;
                let lo = from_hex(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized"})),
    )
        .into_response()
}

/// The 403 detail a write-tier key gets on an operator route.
pub(crate) const WRITE_TIER_REFUSAL: &str = "write-tier token: route not allowed";

/// 403 for a write-tier key on a route that is for operators.
pub(crate) fn refuse_write_tier(identity: &AuthIdentity) -> Option<Response> {
    matches!(identity, AuthIdentity::Write).then(|| forbidden(WRITE_TIER_REFUSAL))
}

/// 403 response for role-bound token operations that are denied in v1.
fn forbidden(detail: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error": detail}))).into_response()
}

/// Convert a `mask_for_role` error into an HTTP response.
///
/// - `GraphError::Corrupt` → 500: the roles sidecar is poisoned; the server
///   refuses all role-token requests until the file is fixed and the DB is
///   re-opened.  Full-access tokens are unaffected.
/// - `GraphError::KeyNotFound` with a `role:` prefix → 401: the token is bound
///   to a role that does not exist in the DB at this request time.
fn role_mask_err(e: GraphError) -> Response {
    match e {
        GraphError::Corrupt { detail } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("roles misconfigured: {detail}")})),
        )
            .into_response(),
        GraphError::KeyNotFound { key } if key.starts_with("role:") => unauthorized(),
        other => graph_err(other),
    }
}

fn err_response(detail: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": detail.into()})),
    )
        .into_response()
}

/// 503 response for a store held by another process's write lock.
///
/// `Busy` is transient and nothing was written, so retrying is always safe.
/// 503 plus `Retry-After` is what standard client retry middleware keys on;
/// a 4xx would tell the client the request itself was malformed.
fn busy_response(detail: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, HeaderValue::from_static("1"))],
        Json(json!({ "error": detail })),
    )
        .into_response()
}

fn graph_err(e: GraphError) -> Response {
    match e {
        // §4.3: role-scoped write denials map to 403 with the verbatim reason
        // string (Display delegates to reason, so .to_string() == reason).
        GraphError::RoleWriteDenied { reason } => forbidden(&reason),
        // Ledger row 12. `role_mask_err` already answered 500 for `Corrupt`
        // while this catch-all answered 400, so one damaged store answered
        // differently depending on whether the role-mask memo was warm. A
        // corrupt store is a server-side condition: 400 tells a caller to change
        // an input, and no input they can send will help. The message is
        // deliberately `to_string()` — identical to what the catch-all produced
        // — so only the status moves.
        corrupt @ GraphError::Corrupt { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": corrupt.to_string() })),
        )
            .into_response(),
        GraphError::QueryError { detail } | GraphError::IngestError { detail } => {
            err_response(detail)
        }
        // Transient: another process holds the cross-process write lock.
        busy @ GraphError::Busy { .. } => busy_response(busy.to_string()),
        // The data directory is not writable by the server's user. That is the
        // operator's to fix and no request can change it, so it is a 500, not
        // the 400 the catch-all would give. The full message (paths, uids) goes
        // to the log; the client gets a fixed one.
        GraphError::Unwritable { path, detail, .. } => {
            tracing::error!(path = %path, "{detail}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": GraphError::UNWRITABLE_CLIENT_MESSAGE })),
            )
                .into_response()
        }
        // A unique-constraint clash is a conflict with stored state, like a
        // duplicate key. The text names the constraint and field only.
        clash @ GraphError::ConstraintViolation { .. } => (
            StatusCode::CONFLICT,
            Json(json!({ "error": clash.to_string() })),
        )
            .into_response(),
        // A value over the store's size limit: the request is too large for
        // what it stores, not malformed.
        big @ GraphError::ValueTooLarge { .. } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": big.to_string() })),
        )
            .into_response(),
        other => err_response(other.to_string()),
    }
}

fn key_not_found(key: String) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": GraphError::KeyNotFound { key }.to_string()})),
    )
        .into_response()
}

fn conflict_response(key: String) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": GraphError::DuplicateKey { key }.to_string()})),
    )
        .into_response()
}

fn json_ok(value: Js) -> Response {
    (StatusCode::OK, Json(value)).into_response()
}

fn ingest_options(v: Option<&Js>) -> Result<IngestOptions, String> {
    let Some(v) = v else {
        return Ok(IngestOptions::default());
    };
    if v.is_null() {
        return Ok(IngestOptions::default());
    }
    let obj = v
        .as_object()
        .ok_or_else(|| "options must be an object".to_string())?;
    let mut opts = IngestOptions::default();
    if let Some(kf) = obj.get("key_field") {
        opts.key_field = kf
            .as_str()
            .ok_or_else(|| "options.key_field must be a string".to_string())?
            .to_string();
    }
    if let Some(fk) = obj.get("auto_fk") {
        if fk == &Js::Bool(false) || fk.as_str() == Some("off") {
            opts.auto_fk = AutoFk::Off;
        } else if let Some(m) = fk.as_object() {
            let suf = m
                .get("suffix")
                .and_then(Js::as_str)
                .ok_or_else(|| "options.auto_fk.suffix must be a string".to_string())?;
            opts.auto_fk = AutoFk::Auto {
                suffix: suf.to_string(),
            };
        } else {
            return Err("options.auto_fk must be false, \"off\", or {suffix}".into());
        }
    }
    Ok(opts)
}

/// Format a `ResultSet` as Arrow IPC or JSON depending on `format`.
fn format_query_result(rs: ResultSet, format: &str) -> Response {
    match format {
        "" => match to_ipc_bytes(&rs) {
            Ok(bytes) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/vnd.apache.arrow.stream")],
                bytes,
            )
                .into_response(),
            Err(e) => err_response(e),
        },
        "json" => json_ok(result_set_json(&rs)),
        other => err_response(format!("unknown format: {other}")),
    }
}

async fn query(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    read_only_key: Option<Extension<ReadOnlyKey>>,
    Query(qs): Query<BTreeMap<String, String>>,
    Json(body): Json<Js>,
) -> Response {
    let cypher = match body.get("cypher").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing cypher"),
    };
    let params = match params_from_json(body.get("params")) {
        Ok(p) => p,
        Err(e) => return err_response(e),
    };
    let format = qs.get("format").map(String::as_str).unwrap_or("");

    // Optional time-travel: `as_of` is a 0-based WAL commit index. The query
    // runs against the graph as it existed at that commit (read-only).
    let as_of = match body.get("as_of") {
        None | Some(Js::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n),
            None => return err_response("as_of must be a non-negative integer commit index"),
        },
    };

    // Parse client-supplied mask (optional array of node keys).
    let mask_keys: Option<Vec<String>> = match body.get("mask") {
        None | Some(Js::Null) => None,
        Some(Js::Array(arr)) => {
            let mut keys = Vec::with_capacity(arr.len());
            for v in arr {
                match v.as_str() {
                    Some(s) => keys.push(s.to_string()),
                    None => return err_response("mask must be an array of strings"),
                }
            }
            Some(keys)
        }
        Some(_) => return err_response("mask must be an array of strings"),
    };

    // The second visibility axis. `namespace` is a leg that **intersects**
    // whatever restriction the identity and the body already impose — a role
    // token's binding, a client mask, both — so it can only narrow. A role bound
    // to namespaces honours them with no `namespace` in the body; one outside
    // the binding is the empty intersection, never the union.
    let namespace = match namespace_arg(body.get("namespace")) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };

    // Stub mode discloses node existence, which is exactly the question an
    // as-of read is asking. The two do not compose.
    if as_of.is_some()
        && body
            .get("stub_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        return err_response("as_of (time-travel) does not compose with stub_hidden");
    }

    // Role token: write Cypher routes to query_write_authz (scope + mask
    // resolved under the write lock, §5 discipline).  Read Cypher uses the
    // lock-free epoch snapshot path unchanged.
    if let AuthIdentity::Role(ref role_name) = identity {
        let is_write = match is_write_query(&cypher) {
            Ok(b) => b,
            Err(e) => return err_response(e),
        };
        if is_write && read_only_key.is_some() {
            return forbidden(READ_TIER_REFUSAL);
        }
        if as_of.is_some() && is_write {
            return err_response("as_of (time-travel) queries are read-only");
        }
        // `mask` and `namespace` are read guards: both documented as making the
        // request a read, and the full-token branch below enforces that by
        // routing anything carrying either to `query_masked`, which refuses a
        // write. The role branch used to check `is_write` first and never look
        // at them, so a client sending a write *with* a namespace as its guard
        // got the write executed instead of the 400 the docs promise. Not a
        // widening — `check_single_op_authz` still gates the write — but the
        // guard the caller asked for was not applied.
        if is_write && (mask_keys.is_some() || namespace.is_some()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "masked queries are read-only"})),
            )
                .into_response();
        }
        if is_write {
            let role = role_name.clone();
            let cypher_c = cypher.clone();
            let params_c = params.clone();
            let db = state.db.clone();
            return match blocking_write(move || {
                db.write().query_write_authz(&role, &cypher_c, &params_c)
            })
            .await
            {
                Ok(rs) => format_query_result(rs, format),
                Err(resp) => resp,
            };
        }
        // Time travel: the role's keys and labels are resolved against the
        // graph as it was at `commit`, and a client mask intersects them
        // there.  The role *definition* is the current one — `roles.json` is a
        // sidecar and is never a WAL record, so it has no past version.
        if let Some(commit) = as_of {
            let scope = match mask_keys {
                Some(ref keys) => AsOfScope::RoleAndKeys(role_name, keys),
                None => AsOfScope::Role(role_name),
            };
            let g = state.db.read();
            let out = match namespace.as_deref() {
                Some(ns) => g.query_at_scoped_in_namespace(commit, &cypher, &params, scope, ns),
                None => g.query_at_scoped(commit, &cypher, &params, scope),
            };
            return match out {
                Ok(rs) => format_query_result(rs, format),
                Err(e) => role_mask_err(e),
            };
        }
        let snap = state.db.reader();
        let role_mask = match snap.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        let effective_mask = if let Some(ref keys) = mask_keys {
            // Client mask intersects role mask — never widens visibility.
            let client_mask = NodeMask::from_ids(keys.iter().filter_map(|k| snap.resolve_key(k)));
            role_mask.intersect(&client_mask)
        } else {
            role_mask
        };
        // And the namespace leg intersects whatever that came to, on the same
        // snapshot the query runs against.
        let effective_mask = match namespace.as_deref() {
            Some(ns) => match snap.mask_for_namespace(ns) {
                Ok(ns_mask) => effective_mask.intersect(&ns_mask),
                Err(e) => return graph_err(e),
            },
            None => effective_mask,
        };
        return match snap.query_masked(&cypher, &params, &effective_mask) {
            Ok(rs) => format_query_result(rs, format),
            Err(GraphError::MaskedReadOnly) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "masked queries are read-only"})),
            )
                .into_response(),
            Err(e) => graph_err(e),
        };
    }

    // Full token: existing paths below.

    // When a client-supplied mask is present, route to query_masked (read-only).
    // The mask and query_masked resolve against one snapshot so the mask and
    // the query see the same graph.
    //
    // `stub_hidden: true` opts into MaskMode::Stub for the mask; Cypher query
    // behaviour is identical in both modes (hidden nodes are excluded from
    // query results regardless of mode).
    if mask_keys.is_some() || namespace.is_some() {
        // Time travel: the allow-list resolves against the as-of graph, so a
        // key that did not exist at `commit` resolves to nothing.
        if let Some(commit) = as_of {
            let g = state.db.read();
            let out = match (&mask_keys, namespace.as_deref()) {
                (Some(keys), Some(ns)) => g.query_at_scoped_in_namespace(
                    commit,
                    &cypher,
                    &params,
                    AsOfScope::Keys(keys),
                    ns,
                ),
                (Some(keys), None) => {
                    g.query_at_scoped(commit, &cypher, &params, AsOfScope::Keys(keys))
                }
                (None, Some(ns)) => {
                    g.query_at_scoped(commit, &cypher, &params, AsOfScope::Namespace(ns))
                }
                (None, None) => unreachable!("one of the two is Some in this branch"),
            };
            return match out {
                Ok(rs) => format_query_result(rs, format),
                Err(e) => graph_err(e),
            };
        }
        let stub_hidden = body
            .get("stub_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let with_mode = |m: NodeMask| {
            if stub_hidden {
                m.with_mode(MaskMode::Stub)
            } else {
                m
            }
        };
        // The mask and the query resolve against one lock-free snapshot, so
        // they see the same graph without holding the read lock for the query.
        let snap = state.db.reader();
        let key_mask = mask_keys
            .as_ref()
            .map(|keys| snapshot_mask_from_keys(&snap, keys.iter().map(String::as_str)));
        let mask = match (key_mask, namespace.as_deref()) {
            (Some(keys), Some(ns)) => match snap.mask_for_namespace(ns) {
                Ok(ns_mask) => keys.intersect(&ns_mask),
                Err(e) => return graph_err(e),
            },
            (Some(keys), None) => keys,
            (None, Some(ns)) => match snap.mask_for_namespace(ns) {
                Ok(ns_mask) => ns_mask,
                Err(e) => return graph_err(e),
            },
            (None, None) => unreachable!("one of the two is Some in this branch"),
        };
        return masked_query_response(
            snap.query_masked(&cypher, &params, &with_mode(mask)),
            format,
        );
    }

    // Detect write statements to dispatch to the correct lock.
    // Write statements (CREATE / MATCH…SET / MATCH…DELETE / MERGE) need the
    // write lock so mutations flow through WAL + rule engine with fsync before
    // the response is sent.  Read queries (MATCH … RETURN …) use the
    // lock-free reader (see `read_query`).
    let is_write = match is_write_query(&cypher) {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };

    if as_of.is_some() && is_write {
        return err_response("as_of (time-travel) queries are read-only");
    }

    let rs = if is_write {
        let db = state.db.clone();
        match blocking_write(move || db.write().query_write(&cypher, &params)).await {
            Ok(rs) => rs,
            Err(resp) => return resp,
        }
    } else if let Some(commit) = as_of {
        match state.db.read().query_at(commit, &cypher, &params) {
            Ok(rs) => rs,
            Err(e) => return graph_err(e),
        }
    } else {
        match read_query(&state.db, &cypher, &params) {
            Ok(rs) => rs,
            Err(e) => return graph_err(e),
        }
    };

    format_query_result(rs, format)
}

// ── /schema/indexes and /schema/constraints (TY-11) ─────────────────────────
//
// Each route runs what the matching Cypher DDL statement runs
// (`GraphDb::schema_*`, `SHOW …`), so HTTP, Cypher and the CLI cannot
// disagree: a row has the fields and values `SHOW` gives it, a role-bound
// caller sees what its `SHOW` shows, and a write is refused to the callers a
// DDL write is refused to.

/// The rows of `SHOW INDEXES` / `SHOW CONSTRAINTS` as objects, narrowed to
/// what `identity` can see exactly as the same statement through `/query` is.
fn schema_listing_response(state: &AppState, identity: &AuthIdentity, statement: &str) -> Response {
    let params = BTreeMap::new();
    let rs = match identity {
        AuthIdentity::Role(role_name) => {
            let snap = state.db.reader();
            let role_mask = match snap.mask_for_role(role_name) {
                Ok(m) => m,
                Err(e) => return role_mask_err(e),
            };
            snap.query_masked(statement, &params, &role_mask)
        }
        AuthIdentity::Full | AuthIdentity::Write => state.db.reader().query(statement, &params),
        AuthIdentity::Anonymous => return forbidden("this endpoint requires a token"),
    };
    let rs = match rs {
        Ok(rs) => rs,
        Err(e) => return graph_err(e),
    };
    let columns: Vec<&String> = rs.columns().iter().collect();
    let rows: Vec<Js> = (0..rs.len())
        .map(|i| {
            let obj: serde_json::Map<String, Js> = columns
                .iter()
                .zip(rs.row(i))
                .map(|(col, cell)| {
                    let v = cell.as_ref().map_or(Js::Null, crate::json::value_to_json);
                    ((*col).clone(), v)
                })
                .collect();
            Js::Object(obj)
        })
        .collect();
    json_ok(Js::Array(rows))
}

async fn schema_indexes_list(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    schema_listing_response(&state, &identity, "SHOW INDEXES")
}

async fn schema_constraints_list(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    schema_listing_response(&state, &identity, "SHOW CONSTRAINTS")
}

/// 403 for a role-bound token on a schema write, as a DDL statement gives it.
const SCHEMA_ROLE_REFUSAL: &str =
    "role-bound token: schema statements (CREATE/DROP INDEX, CREATE/DROP \
     CONSTRAINT) are not permitted";

/// The refusal a schema write gets for `identity`, if any. A read-tier key never
/// reaches here (the auth middleware refuses it first), but carries a role.
fn refuse_schema_write(identity: &AuthIdentity) -> Option<Response> {
    match identity {
        AuthIdentity::Role(_) => Some(forbidden(SCHEMA_ROLE_REFUSAL)),
        AuthIdentity::Anonymous => Some(forbidden("this endpoint requires a token")),
        AuthIdentity::Full | AuthIdentity::Write => None,
    }
}

/// A schema write's error as HTTP, by the kind the engine decided from the
/// state it found: a name nothing answers to is 404, a collision with what
/// exists (a taken name or pair, an index a constraint backs, existing
/// duplicates, a view-owned property) is 409, and a malformed request is 400.
fn schema_err(e: SchemaError) -> Response {
    let status = match e.kind {
        SchemaErrorKind::NotFound => StatusCode::NOT_FOUND,
        SchemaErrorKind::Conflict => StatusCode::CONFLICT,
        SchemaErrorKind::Invalid => StatusCode::BAD_REQUEST,
        SchemaErrorKind::Other => return graph_err(e.error),
    };
    let detail = match e.error {
        GraphError::QueryError { detail } | GraphError::RuleInvalid { detail } => detail,
        other => other.to_string(),
    };
    (status, Json(json!({ "error": detail }))).into_response()
}

/// The refusal for a schema body that is not an object, or carries a field
/// outside `allowed`, naming it.
fn schema_body_check(body: &Js, allowed: &[&str]) -> std::result::Result<(), String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "the body must be a JSON object".to_string())?;
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(unknown) => Err(format!(
            "unknown field '{unknown}'; this route takes: {}",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

/// A required, non-empty string field of a schema request body.
fn schema_text<'a>(body: &'a Js, field: &str) -> std::result::Result<&'a str, String> {
    match body.get(field).and_then(Js::as_str) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!(
            "{field} is required and must be a non-empty string"
        )),
    }
}

/// An optional `name`: absent or null is `None`; otherwise it must be an
/// identifier, so every surface (HTTP path, Cypher `DROP INDEX name`) can
/// address it.
fn schema_name(body: &Js) -> std::result::Result<Option<&str>, String> {
    match body.get("name") {
        None | Some(Js::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) if is_schema_identifier(s) => Ok(Some(s)),
            _ => Err(
                "name must be a string of letters, digits and underscores, not starting with a digit"
                    .to_string(),
            ),
        },
    }
}

fn is_schema_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn schema_index_create(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    if let Some(refusal) = refuse_schema_write(&identity) {
        return refusal;
    }
    if let Err(e) = schema_body_check(&body, &["name", "label", "property", "kind"]) {
        return err_response(e);
    }
    let (label, property) = match (schema_text(&body, "label"), schema_text(&body, "property")) {
        (Ok(l), Ok(p)) => (l.to_string(), p.to_string()),
        (Err(e), _) | (_, Err(e)) => return err_response(e),
    };
    let name = match schema_name(&body) {
        Ok(n) => n.map(str::to_string),
        Err(e) => return err_response(e),
    };
    let kind =
        match body.get("kind") {
            None | Some(Js::Null) => "range",
            Some(v) => match v.as_str() {
                Some(k @ ("range" | "equality")) => k,
                _ => return err_response(
                    "kind must be \"range\" or \"equality\"; full-text indexes are not created \
                     over this route",
                ),
            },
        };
    let kind = kind.to_string();
    let db = state.db.clone();
    let created = tokio::task::spawn_blocking(move || {
        let mut g = db.write();
        match kind.as_str() {
            "equality" => g.schema_create_equality_index(name.as_deref(), &label, &property),
            _ => g.schema_create_range_index(name.as_deref(), &label, &property),
        }
        .map(|listed| (listed, kind, label, property))
    })
    .await;
    match created {
        Ok(Ok((listed, kind, label, property))) => (
            StatusCode::CREATED,
            Json(json!({
                "name": listed, "type": kind, "label": label,
                "property": property, "state": "online"
            })),
        )
            .into_response(),
        Ok(Err(e)) => schema_err(e),
        Err(_) => err_response("write task panicked"),
    }
}

async fn schema_index_drop(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(name): Path<String>,
) -> Response {
    if let Some(refusal) = refuse_schema_write(&identity) {
        return refusal;
    }
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.write().schema_drop_index(&name)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => schema_err(e),
        Err(_) => err_response("write task panicked"),
    }
}

async fn schema_constraint_create(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    if let Some(refusal) = refuse_schema_write(&identity) {
        return refusal;
    }
    if let Err(e) = schema_body_check(&body, &["name", "label", "property", "type"]) {
        return err_response(e);
    }
    let (label, property) = match (schema_text(&body, "label"), schema_text(&body, "property")) {
        (Ok(l), Ok(p)) => (l.to_string(), p.to_string()),
        (Err(e), _) | (_, Err(e)) => return err_response(e),
    };
    // A constraint needs a name (Cypher DDL requires one), so there is none to invent.
    let name = match schema_name(&body) {
        Ok(Some(n)) => n.to_string(),
        Ok(None) => return err_response("name is required for a constraint"),
        Err(e) => return err_response(e),
    };
    match body.get("type") {
        None | Some(Js::Null) => {}
        Some(v) if v.as_str() == Some("unique") => {}
        Some(_) => return err_response("type must be \"unique\", the only constraint type"),
    }
    let db = state.db.clone();
    let reply = json!({
        "name": name, "type": "unique", "label": label, "property": property
    });
    match tokio::task::spawn_blocking(move || {
        db.write()
            .schema_create_constraint(&name, &label, &property)
    })
    .await
    {
        Ok(Ok(())) => (StatusCode::CREATED, Json(reply)).into_response(),
        Ok(Err(e)) => schema_err(e),
        Err(_) => err_response("write task panicked"),
    }
}

async fn schema_constraint_drop(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(name): Path<String>,
) -> Response {
    if let Some(refusal) = refuse_schema_write(&identity) {
        return refusal;
    }
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.write().schema_drop_constraint(&name)).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => schema_err(e),
        Err(_) => err_response("write task panicked"),
    }
}

/// An unmasked, live (non-`as_of`) read query, on the lock-free reader. An
/// equality lookup on a declared index is served from the snapshot's own
/// index, so this holds for indexed stores too.
fn read_query(
    db: &SharedDb,
    cypher: &str,
    params: &BTreeMap<String, Value>,
) -> mushroomdb::Result<ResultSet> {
    db.reader().query(cypher, params)
}

/// The response for a full-token masked `/query`, from either path.
fn masked_query_response(out: mushroomdb::Result<ResultSet>, format: &str) -> Response {
    match out {
        Ok(rs) => format_query_result(rs, format),
        Err(GraphError::MaskedReadOnly) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "masked queries are read-only"})),
        )
            .into_response(),
        Err(e) => graph_err(e),
    }
}

/// [`NodeMask::from_keys`] against a snapshot: keys it does not hold are
/// ignored, and the mode is `Omit`.
fn snapshot_mask_from_keys<'a>(
    snap: &ReaderSnapshot,
    keys: impl IntoIterator<Item = &'a str>,
) -> NodeMask {
    NodeMask::from_ids(keys.into_iter().filter_map(|k| snap.resolve_key(k)))
}

/// The client mask a full-token `/node/*` route reads from `mask=` and
/// `stub_hidden=`, resolved against `snap`.
fn snapshot_client_mask(snap: &ReaderSnapshot, mask_param: &str, stub_hidden: bool) -> NodeMask {
    let keys = mask_param
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let m = snapshot_mask_from_keys(snap, keys);
    if stub_hidden {
        m.with_mode(MaskMode::Stub)
    } else {
        m
    }
}

/// `GET /stats` — store-wide counts, including the `namespaces` roster.
///
/// Role tokens are denied, as they always were, and that is also what settles
/// the namespace roster on this surface: the roster names every namespace and
/// its live count, which is a list of other tenants, so narrowing it for a role
/// token would be a second answer to a question this route never answers for one
/// at all. A full-access token sees the whole roster; a tenant-scoped client gets
/// 403 here and asks MCP `stats` with its `role`, which narrows.
async fn stats(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    // v1: deny role tokens — raw counts leak graph size beyond the role's
    // subgraph, and `namespaces` would name every other tenant.
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /stats requires a full-access token");
    }
    if let Some(refusal) = refuse_write_tier(&identity) {
        return refusal;
    }
    let snap = {
        let g = state.db.read();
        g.stats()
    };
    match serde_json::to_value(&snap) {
        Ok(v) => json_ok(v),
        Err(e) => err_response(e.to_string()),
    }
}

async fn metrics(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    // Mirror /stats auth: deny role tokens — counters leak graph size.
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /metrics requires a full-access token");
    }
    if let Some(refusal) = refuse_write_tier(&identity) {
        return refusal;
    }
    let snap = state.db.reader();
    let (nodes_live, nodes_tombstoned, edges) = snap.counts();
    let commit_seq = snap.commit_seq();
    let wal_size_bytes = snap.wal_size_bytes().ok();
    let slow_snap = snap.slow_query_snapshot();
    let uptime_s = state.started_at.elapsed().as_secs();
    let slow_entries: Vec<Js> = slow_snap
        .last
        .iter()
        .map(|e| {
            json!({
                "ms": e.ms,
                "query": e.query,
                "at_commit": e.at_commit,
            })
        })
        .collect();
    json_ok(json!({
        "nodes_live": nodes_live,
        "nodes_tombstoned": nodes_tombstoned,
        "edges": edges,
        "commit_seq": commit_seq,
        "wal_size_bytes": wal_size_bytes,
        "rss_bytes": rss_bytes(),
        "uptime_s": uptime_s,
        "slow_queries": {
            "threshold_ms": slow_snap.threshold_ms,
            "count": slow_snap.count,
            "last": slow_entries,
        },
    }))
}

/// Resident set size of the current process in bytes.
///
/// macOS: mach task_info (MACH_TASK_BASIC_INFO).
/// Linux: /proc/self/statm RSS pages × page size.
/// Other: always returns None.
///
/// Never panics; returns None on any failure.
fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // MACH_TASK_BASIC_INFO flavor = 20.
        // struct mach_task_basic_info { u64 virtual, u64 resident,
        //   u64 resident_max, [u32;2] user_time, [u32;2] sys_time,
        //   i32 policy, i32 suspend_count } = 48 bytes = 12 natural_t(u32)s.
        const MACH_TASK_BASIC_INFO: u32 = 20;
        const MACH_TASK_BASIC_INFO_COUNT: u32 = 12;

        #[repr(C)]
        struct MachTaskBasicInfo {
            virtual_size: u64,
            resident_size: u64,
            resident_size_max: u64,
            user_time: [u32; 2],
            system_time: [u32; 2],
            policy: i32,
            suspend_count: i32,
        }

        extern "C" {
            fn mach_task_self() -> u32;
            fn task_info(
                target_task: u32,
                flavor: u32,
                task_info_out: *mut std::ffi::c_void,
                task_info_cnt: *mut u32,
            ) -> i32;
        }

        let mut info: MachTaskBasicInfo = unsafe { std::mem::zeroed() };
        let mut count = MACH_TASK_BASIC_INFO_COUNT;
        let ret = unsafe {
            task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                &mut info as *mut _ as *mut _,
                &mut count,
            )
        };
        if ret != 0 {
            return None; // KERN_SUCCESS = 0
        }
        return Some(info.resident_size);
    }

    #[cfg(target_os = "linux")]
    {
        // /proc/self/statm: "vsize rss ..." in pages.
        let content = std::fs::read_to_string("/proc/self/statm").ok()?;
        let mut parts = content.split_whitespace();
        let _vsize = parts.next()?;
        let rss_pages: u64 = parts.next()?.parse().ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return None;
        }
        return Some(rss_pages * page_size as u64);
    }

    #[allow(unreachable_code)]
    None
}

async fn ingest(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    let label = match body.get("label").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing label"),
    };
    let rows = match body.get("rows") {
        Some(r) => r,
        None => return err_response("missing rows"),
    };
    let mut converted = match json_to_rows(rows) {
        Ok(c) => c,
        Err(e) => return graph_err(e),
    };
    let opts = match ingest_options(body.get("options")) {
        Ok(o) => o,
        Err(e) => return err_response(e),
    };
    // `namespace` applies to every node this call creates.
    let namespace = match namespace_arg(body.get("namespace")) {
        Ok(n) => n,
        Err(e) => return err_response(e),
    };
    if let Err(e) = stamp_namespace(&mut converted.rows, namespace.as_deref()) {
        return err_response(e);
    }
    let taken = std::mem::take(&mut converted.rows);
    let edges = match body.get("edges") {
        None | Some(Js::Null) => Vec::new(),
        Some(raw) => match parse_ingest_edges(raw) {
            Ok(e) => e,
            Err(e) => return err_response(e),
        },
    };
    let db = state.db.clone();

    // Role token: route to ingest_with_edges_authz (scope + mask resolved under
    // the write lock, §5 discipline; §7.3 — create_labels required, enforced by
    // the engine's per-op authz check inside commit_logged_batch).
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.write()
                .ingest_with_edges_authz(&role, &label, taken, &opts, &edges)
        })
        .await
        {
            Ok(r) => {
                let report = converted.into_report(r);
                match serde_json::to_value(&report) {
                    Ok(v) => json_ok(v),
                    Err(e) => err_response(e.to_string()),
                }
            }
            Err(resp) => resp,
        };
    }

    let report =
        match blocking_write(move || db.write().ingest_with_edges(&label, taken, &opts, &edges))
            .await
        {
            Ok(r) => converted.into_report(r),
            Err(resp) => return resp,
        };
    match serde_json::to_value(&report) {
        Ok(v) => json_ok(v),
        Err(e) => err_response(e.to_string()),
    }
}

/// `GET /suggest` — profile the database and return rule suggestions.
///
/// # Locking and blocking strategy
///
/// `suggest_rules_with_config` is CPU-intensive and synchronous. Running it on a
/// Tokio worker thread would starve the executor. This handler offloads the work to
/// `tokio::task::spawn_blocking`, which uses the blocking thread-pool. The
/// `std::sync::RwLock` read guard is acquired and held inside the blocking task —
/// reads don't block other reads; writes wait for the guard to drop. The global
/// budget (`SuggestConfig::global_budget_ms`, default 5 s) caps lock-hold time.
async fn suggest(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    // v1: deny role tokens — suggest scans the full graph and would reveal
    // existence of nodes outside the role's mask.
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /suggest requires a full-access token");
    }
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || {
        let config = SuggestConfig::default();
        db.read()
            .suggest_rules_with_config(&config, SUGGEST_DEFAULT_SEED)
    })
    .await
    {
        Ok(report) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Err(_) => err_response("suggest task panicked"),
    }
}

async fn create_rule(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: writes are not permitted");
    }
    let def = match rule_def_from_json(body) {
        Ok(d) => d,
        Err(e) => return err_response(e),
    };
    let name = def.name.clone();
    let db = state.db.clone();
    if let Err(resp) = blocking_write(move || db.write().create_rule(def)).await {
        return resp;
    }
    // A corpus too large to index in one commit leaves the rule installed but
    // deriving nothing, so the route says "accepted", not "done", and hands
    // back the progress the caller can poll on `GET /stats`.
    //
    // Read under a *separate* lock from the write above, so a concurrent pump
    // can finish the build in between and this route then answers 200 for a
    // create that really did defer. Benign — 200 means "the edges are there",
    // which by then they are — and the caller's contract is to poll `GET /stats`
    // either way.
    let building = state
        .db
        .read()
        .builds_in_progress()
        .into_iter()
        .find(|b| b.rule == name);
    match building {
        Some(b) => (
            StatusCode::ACCEPTED,
            Json(json!({"rule": name, "building": {"indexed": b.indexed, "total": b.total}})),
        )
            .into_response(),
        None => json_ok(json!({"ok": true, "name": name})),
    }
}

async fn explain(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    // v1: deny role tokens — explain reveals hidden-node linkage through rules.
    if let AuthIdentity::Role(_) = identity {
        return forbidden(
            "role-bound token: /explain requires a full-access token \
             (v1: explain may reveal hidden-node linkage; revisit when stubs land)",
        );
    }
    let a = match qs.get("a") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err_response("missing query param a"),
    };
    let b = match qs.get("b") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return err_response("missing query param b"),
    };
    let out = {
        let g = state.db.read();
        g.explain(&a, &b)
    };
    match out {
        Ok(v) => match serde_json::to_value(&v) {
            Ok(j) => json_ok(j),
            Err(e) => err_response(e.to_string()),
        },
        Err(e) => graph_err(e),
    }
}

async fn node_info(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    if let AuthIdentity::Role(ref role_name) = identity {
        // Role-token path: hard-coded Omit mode; hidden keys are indistinguishable
        // from absent keys.  `stub_hidden` query param is silently ignored here —
        // role paths must NEVER produce stubs (RBAC invariant).
        let snap = state.db.reader();
        let role_mask = match snap.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        if !snap
            .resolve_key(&key)
            .is_some_and(|id| role_mask.contains_id(id))
        {
            return key_not_found(key);
        }
        return match snap.node_info(&key) {
            Some(info) => json_ok(node_info_json(&info)),
            None => key_not_found(key),
        };
    }

    // Full-token path: optional client mask + stub_hidden via query params.
    // `mask=key1,key2` — comma-separated visible keys (empty string = no mask).
    // `stub_hidden=true` — opt into MaskMode::Stub for this request.
    let mask_param = qs.get("mask").map(String::as_str).unwrap_or("").trim();
    if !mask_param.is_empty() {
        let stub_hidden = qs
            .get("stub_hidden")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let snap = state.db.reader();
        if let Err(e) = snap.ensure_readable() {
            return graph_err(e);
        }
        let mask = snapshot_client_mask(&snap, mask_param, stub_hidden);
        return match snap.node_info_masked(&key, &mask) {
            Some(mushroomdb::MaskedNodeResult::Visible(info)) => json_ok(node_info_json(&info)),
            Some(mushroomdb::MaskedNodeResult::Restricted) => {
                json_ok(crate::json::stub_node_json(&key))
            }
            None => key_not_found(key),
        };
    }

    let snap = state.db.reader();
    if let Err(e) = snap.ensure_readable() {
        return graph_err(e);
    }
    match snap.node_info(&key) {
        Some(info) => json_ok(node_info_json(&info)),
        None => key_not_found(key),
    }
}

async fn node_edges(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    if let AuthIdentity::Role(ref role_name) = identity {
        // Role-token path: hard-coded Omit mode.  `stub_hidden` query param is
        // silently ignored — role paths must NEVER produce stubs (RBAC invariant).
        //
        // The subject check and the hidden-endpoint filter live in
        // `ReaderSnapshot::node_edges_scoped`, so every scoped caller gets them,
        // not only this route.
        let snap = state.db.reader();
        let role_mask = match snap.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        return match snap.node_edges_scoped(&key, &role_mask) {
            Ok(edges) => json_ok(node_edges_json(&edges)),
            Err(GraphError::KeyNotFound { key }) => key_not_found(key),
            Err(e) => graph_err(e),
        };
    }

    // Full-token path: optional client mask + stub_hidden via query params.
    let mask_param = qs.get("mask").map(String::as_str).unwrap_or("").trim();
    if !mask_param.is_empty() {
        let stub_hidden = qs
            .get("stub_hidden")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let snap = state.db.reader();
        if let Err(e) = snap.ensure_readable() {
            return graph_err(e);
        }
        let mask = snapshot_client_mask(&snap, mask_param, stub_hidden);
        return match snap.node_edges_masked(&key, &mask) {
            Ok(edges) => json_ok(crate::json::masked_edges_json(&edges)),
            Err(GraphError::KeyNotFound { key }) => key_not_found(key),
            Err(e) => graph_err(e),
        };
    }

    match state.db.reader().node_edges(&key) {
        Ok(edges) => json_ok(node_edges_json(&edges)),
        Err(GraphError::KeyNotFound { key }) => key_not_found(key),
        Err(e) => graph_err(e),
    }
}

async fn neighborhood(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    let depth = match resolve_neighborhood_depth(qs.get("depth").map(String::as_str)) {
        Ok(d) => d,
        Err(e) => return err_response(e),
    };
    let dir = match qs.get("dir").map(String::as_str).unwrap_or("both") {
        s if s.eq_ignore_ascii_case("out") => Dir::Out,
        s if s.eq_ignore_ascii_case("in") => Dir::In,
        s if s.eq_ignore_ascii_case("both") => Dir::Both,
        other => return err_response(format!("unknown dir: {other}")),
    };
    let edge_type_names: Option<Vec<String>> = qs.get("edge_types").map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    });
    let etype_refs: Option<Vec<&str>> = edge_type_names
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    if let AuthIdentity::Role(ref role_name) = identity {
        // Lock-free epoch snapshot: mask and neighborhood BFS on same frozen state
        // (constraint 2 — RBAC mask coherence).
        let snap = state.db.reader();
        let role_mask = match snap.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        // The scoped BFS: the subject is checked first (a hidden key answers as
        // an absent one), and hidden nodes are excluded from results AND cannot
        // be used as traversal intermediaries (never-leak invariant).
        return match snap.neighborhood_scoped(&key, depth, etype_refs.as_deref(), dir, &role_mask) {
            Ok(rs) => json_ok(result_set_json(&rs)),
            Err(GraphError::KeyNotFound { key }) => key_not_found(key),
            Err(e) => graph_err(e),
        };
    }
    // Full-token path: optional client mask + stub_hidden via query params.
    // `mask=key1,key2` — comma-separated visible keys.
    // `stub_hidden=true` — opt into MaskMode::Stub; hidden direct neighbours
    // appear as stub rows (label: null) in the result; BFS does not expand
    // through them in either mode.
    let mask_param = qs.get("mask").map(String::as_str).unwrap_or("").trim();
    if !mask_param.is_empty() {
        let stub_hidden = qs
            .get("stub_hidden")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        let snap = state.db.reader();
        if let Err(e) = snap.ensure_readable() {
            return graph_err(e);
        }
        let mask = snapshot_client_mask(&snap, mask_param, stub_hidden);
        return match snap.neighborhood_masked(&key, depth, etype_refs.as_deref(), dir, &mask) {
            Some(rs) => json_ok(result_set_json(&rs)),
            None => key_not_found(key),
        };
    }

    // Unmasked full-token path (no mask param).
    let snap = state.db.reader();
    if let Err(e) = snap.ensure_readable() {
        return graph_err(e);
    }
    match snap.neighborhood(&key, depth, etype_refs.as_deref(), dir) {
        Some(rs) => json_ok(result_set_json(&rs)),
        None => key_not_found(key),
    }
}

/// `POST /find_similar` — vector kNN. First-class read, not `/algo/*`, so a
/// role token is allowed and a client `mask` intersects the role (never widens).
/// Default `min` is `mushroomdb::FIND_SIMILAR_DEFAULT_MIN` (0.8), as on MCP and
/// in the Python binding. It was 0.0 here through 0.6.
async fn find_similar(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    let field = match body.get("field").and_then(Js::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err_response("missing field"),
    };
    let vector = match parse_find_similar_vector(body.get("vector")) {
        Ok(v) => v,
        Err(e) => return err_response(e),
    };
    let k = match body.get("k") {
        None | Some(Js::Null) => 10usize,
        Some(v) => match v.as_u64() {
            Some(n) => n as usize,
            None => return err_response("k must be a non-negative integer"),
        },
    };
    let min = match body.get("min") {
        None | Some(Js::Null) => mushroomdb::FIND_SIMILAR_DEFAULT_MIN,
        Some(v) => match v.as_f64() {
            Some(n) => n,
            None => return err_response("min must be a number"),
        },
    };
    let label = match body.get("label").and_then(Js::as_str) {
        None | Some("") => None,
        Some(s) => Some(s.to_string()),
    };
    let mask_keys = match parse_mask_keys(body.get("mask")) {
        Ok(m) => m,
        Err(e) => return err_response(e),
    };
    let where_pred = match parse_where_body(body.get("where")) {
        Ok(p) => p,
        Err(detail) => return graph_err(GraphError::QueryError { detail }),
    };
    let exact = match body.get("exact") {
        None | Some(Js::Null) => false,
        Some(v) => match v.as_bool() {
            Some(b) => b,
            None => return err_response("exact must be a boolean"),
        },
    };
    let exact = exact || where_pred.is_some();
    let role_name = match identity {
        AuthIdentity::Role(r) => Some(r),
        AuthIdentity::Full | AuthIdentity::Write => None,
        // Unreachable on this route — `Anonymous` is only ever inserted for
        // `GET /health` — but treated as the narrowest identity rather than
        // the widest, so a future open endpoint cannot inherit full access by
        // omission.
        AuthIdentity::Anonymous => {
            return forbidden("this endpoint requires a token");
        }
    };
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || {
        let g = db.read();
        let role_mask = match role_name.as_deref() {
            Some(role) => Some(g.mask_for_role(role)?),
            None => None,
        };
        let client_mask = mask_keys
            .as_ref()
            .map(|keys| NodeMask::from_keys(&*g, keys.iter().map(String::as_str)));
        let effective = match (role_mask, client_mask) {
            (Some(role), Some(client)) => Some(role.intersect(&client)),
            (Some(role), None) => Some(role),
            (None, Some(client)) => Some(client),
            (None, None) => None,
        };
        g.find_similar_vector_filtered(
            &field,
            label.as_deref(),
            &vector,
            k,
            min,
            effective.as_ref(),
            where_pred.as_ref(),
            exact,
        )
    })
    .await
    {
        Ok(Ok(hits)) => json_ok(json!({ "hits": hits })),
        Ok(Err(e)) => role_mask_err(e),
        Err(_) => err_response("find_similar task panicked"),
    }
}

/// `POST /algo/pagerank` — run PageRank over the unified topology.
///
/// # Locking and blocking strategy
///
/// PageRank is CPU-intensive and synchronous. This handler offloads the work
/// to `tokio::task::spawn_blocking` (blocking thread-pool), over a lock-free
/// [`ReaderSnapshot`], so it holds no guard and never holds a writer up. The
/// `budget_ms` field in [`PageRankConfig`] caps its running time.
async fn algo_pagerank(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // v1: deny role tokens — algo endpoints scan the full graph.
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /algo/* requires a full-access token");
    }
    let config: PageRankConfig = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => return err_response(format!("invalid pagerank config: {e}")),
    };
    if let Err(e) = config.validate() {
        return err_response(format!("invalid pagerank config: {e}"));
    }
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.reader().pagerank(&config)).await {
        Ok(Ok(report)) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Ok(Err(e)) => graph_err(e),
        Err(_) => err_response("pagerank task panicked"),
    }
}

/// `POST /algo/wcc` — weakly-connected components over the unified topology.
///
/// Mirrors `algo_pagerank`: spawn_blocking over a lock-free snapshot,
/// `budget_ms` in config caps running time.
async fn algo_wcc(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /algo/* requires a full-access token");
    }
    let config: WccConfig = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => return err_response(format!("invalid wcc config: {e}")),
    };
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.reader().connected_components(&config)).await {
        Ok(Ok(report)) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Ok(Err(e)) => graph_err(e),
        Err(_) => err_response("wcc task panicked"),
    }
}

/// `POST /algo/degree` — degree centrality over the unified topology.
///
/// Mirrors `algo_pagerank`.
async fn algo_degree(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /algo/* requires a full-access token");
    }
    let config: DegreeConfig = match serde_json::from_value(body) {
        Ok(c) => c,
        Err(e) => return err_response(format!("invalid degree config: {e}")),
    };
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.reader().degree_centrality(&config)).await {
        Ok(Ok(report)) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Ok(Err(e)) => graph_err(e),
        Err(_) => err_response("degree task panicked"),
    }
}

// ── Simple BatchOp-mapped endpoints ──────────────────────────────────────────
//
// These endpoints map 1:1 to BatchOp variants and route through submit_batch
// so concurrent writes share one WAL fsync per drain group, keeping reader
// p95 latency low under write bursts.  Auth checks (role tokens → 403) run
// before enqueue so RBAC enforcement is unchanged.
//
// Complex paths (/query Cypher writes, /ingest bulk JSON) stay on db.write()
// because they are multi-step operations that cannot be pre-expressed as a
// Vec<BatchOp> without redesigning the query executor.

/// Parse a JSON object into a `Vec<(String, Value)>` prop list.
fn props_from_json_obj(v: &serde_json::Value) -> Result<Vec<(String, Value)>, String> {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Err("props must be a JSON object".into()),
    };
    let mut out = Vec::with_capacity(obj.len());
    for (k, val) in obj {
        if let Some(v) = json_to_value(val.clone()).map_err(|e| format!("props {k:?}: {e}"))? {
            out.push((k.clone(), v));
        }
    }
    Ok(out)
}

/// `POST /nodes` — create or upsert a node via the group-commit queue.
///
/// Body: `{"label": "Person", "key": "alice", "props": {"age": 30}}`
async fn create_node(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    let label = match body.get("label").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing label"),
    };
    let key = match body.get("key").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing key"),
    };
    let mut props = match body.get("props") {
        None | Some(Js::Null) => vec![],
        Some(v) => match props_from_json_obj(v) {
            Ok(p) => p,
            Err(e) => return err_response(e),
        },
    };
    // `namespace` is the namespace the node is created in — the same write-once
    // `ns` property, named on the call instead of buried in `props`.
    match namespace_arg(body.get("namespace")) {
        Ok(None) => {}
        Ok(Some(ns)) => {
            let mut row: BTreeMap<String, Value> = props.into_iter().collect();
            if let Err(e) = stamp_namespace_row(&mut row, &ns) {
                return err_response(e);
            }
            props = row.into_iter().collect();
        }
        Err(e) => return err_response(e),
    }
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(role, vec![BatchOp::InsertNode { label, key, props }])
        })
        .await
        {
            Ok((nodes, edges)) => json_ok(json!({"ok": true, "nodes": nodes, "edges": edges})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || db.submit_batch(vec![BatchOp::InsertNode { label, key, props }]))
        .await
    {
        Ok((nodes, edges)) => json_ok(json!({"ok": true, "nodes": nodes, "edges": edges})),
        Err(resp) => resp,
    }
}

/// `DELETE /node/{key}` — delete a node via the group-commit queue.
async fn delete_node(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
) -> Response {
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(role, vec![BatchOp::DeleteNode { key }])
        })
        .await
        {
            Ok(_) => json_ok(json!({"ok": true})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || db.submit_batch(vec![BatchOp::DeleteNode { key }])).await {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

/// `POST /edges` — create an edge via the group-commit queue.
///
/// Body: `{"type": "KNOWS", "src": "alice", "dst": "bob"}`
async fn create_edge(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    let edge_type = match body.get("type").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing type"),
    };
    let src = match body.get("src").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing src"),
    };
    let dst = match body.get("dst").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing dst"),
    };
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(
                role,
                vec![BatchOp::InsertEdge {
                    edge_type,
                    src_key: src,
                    dst_key: dst,
                }],
            )
        })
        .await
        {
            Ok(_) => json_ok(json!({"ok": true})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || {
        db.submit_batch(vec![BatchOp::InsertEdge {
            edge_type,
            src_key: src,
            dst_key: dst,
        }])
    })
    .await
    {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

/// `DELETE /edges/{etype}/{src}/{dst}` — delete an edge via the group-commit queue.
async fn delete_edge(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path((etype, src, dst)): Path<(String, String, String)>,
) -> Response {
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(
                role,
                vec![BatchOp::DeleteEdge {
                    edge_type: etype,
                    src_key: src,
                    dst_key: dst,
                }],
            )
        })
        .await
        {
            Ok(_) => json_ok(json!({"ok": true})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || {
        db.submit_batch(vec![BatchOp::DeleteEdge {
            edge_type: etype,
            src_key: src,
            dst_key: dst,
        }])
    })
    .await
    {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

/// `POST /nodes/{key}/rename` — rename a node's key via the group-commit queue.
///
/// Body: `{"new_key": "alice2"}`
/// Returns 404 on KeyNotFound, 409 on DuplicateKey, 200 on success.
async fn rename_node(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
    Json(body): Json<Js>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: writes are not permitted");
    }
    let new_key = match body.get("new_key").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing new_key"),
    };
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || {
        db.submit_batch(vec![BatchOp::RenameNode {
            old_key: key,
            new_key,
        }])
    })
    .await
    {
        Ok(Ok(_)) => json_ok(json!({"ok": true})),
        Ok(Err(GraphError::KeyNotFound { key })) => key_not_found(key),
        Ok(Err(GraphError::DuplicateKey { key })) => conflict_response(key),
        Ok(Err(e)) => graph_err(e),
        Err(_) => err_response("write task panicked"),
    }
}

/// `POST /edges/upsert` — insert an edge, auto-creating missing endpoints.
///
/// Body: `{"edge_type":"KNOWS","src_key":"alice","dst_key":"bob","placeholder_label":"Person"}`
/// Returns `{"nodes_created": N, "edge_inserted": bool}`.
async fn upsert_edge(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    let edge_type = match body.get("edge_type").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing edge_type"),
    };
    let src_key = match body.get("src_key").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing src_key"),
    };
    let dst_key = match body.get("dst_key").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing dst_key"),
    };
    let placeholder_label = match body.get("placeholder_label").and_then(Js::as_str) {
        Some(s) => s.to_string(),
        None => return err_response("missing placeholder_label"),
    };
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(
                role,
                vec![BatchOp::InsertEdgeUpsert {
                    edge_type,
                    src_key,
                    dst_key,
                    placeholder_label,
                }],
            )
        })
        .await
        {
            Ok((nodes, edges)) => json_ok(json!({
                "nodes_created": nodes,
                "edge_inserted": edges > 0,
            })),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || {
        db.submit_batch(vec![BatchOp::InsertEdgeUpsert {
            edge_type,
            src_key,
            dst_key,
            placeholder_label,
        }])
    })
    .await
    {
        Ok((nodes, edges)) => json_ok(json!({
            "nodes_created": nodes,
            "edge_inserted": edges > 0,
        })),
        Err(resp) => resp,
    }
}

/// `PUT /node/{key}/prop/{field}` — set a property via the group-commit queue.
///
/// Body: `{"value": 42}`
async fn set_node_prop(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path((key, field)): Path<(String, String)>,
    Json(body): Json<Js>,
) -> Response {
    let parsed = body.get("value").map(|v| json_to_value(v.clone()));
    let value = match parsed {
        Some(Err(e)) => return err_response(format!("value: {e}")),
        Some(Ok(Some(v))) => v,
        Some(Ok(None)) | None => {
            return err_response(
                "request body must be a JSON object with a \"value\" field, \
                 e.g. {\"value\": \"SanFrancisco\"} or {\"value\": [\"a\", \"b\"]}",
            )
        }
    };
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(role, vec![BatchOp::SetProp { key, field, value }])
        })
        .await
        {
            Ok(_) => json_ok(json!({"ok": true})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || db.submit_batch(vec![BatchOp::SetProp { key, field, value }]))
        .await
    {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

// ── History endpoints ─────────────────────────────────────────────────────────
//
// These are cold-path diagnostic endpoints. They scan the on-disk WAL and
// must NOT extend ReaderSnapshot — they use db.read() directly per the
// controller ruling (see module doc and task-2-brief.md).
//
// Role masking: node visibility is checked under the SAME read guard as the
// history call (coherent snapshot). A node outside the role's mask responds
// identically to an absent node — no existence oracle.

/// `GET /node/{key}/history` — return the WAL change history for `key`.
///
/// Response: `{ key, history: [{commit, change}], total_commits, horizon }`.
/// `horizon` is the oldest commit history still reaches; events before it were
/// pruned and are not in `history`.
/// Role tokens: if `key` is hidden by the role mask, responds with 404
/// (same shape as querying an absent key — no existence oracle).
async fn node_history_handler(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path(key): Path<String>,
) -> Response {
    if let AuthIdentity::Role(ref role_name) = identity {
        let g = state.db.read();
        let role_mask = match g.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        // Hidden keys must respond identically to absent keys (no oracle).
        if !role_mask.contains_node(&*g, &key) {
            return key_not_found(key);
        }
        let result = match g.node_history(&key) {
            Ok(e) => e,
            Err(e) => return graph_err(e),
        };
        // Filter EdgeAdded/EdgeRemoved entries whose `other` endpoint is hidden.
        // A role token must not learn about hidden nodes via edge history events —
        // mirrors the same protection in `node_edges` (http.rs ~978-989).
        use mushroomdb::HistoryChange;
        let visible = mushroomdb::HistoryResult {
            total_commits: result.total_commits,
            horizon: result.horizon,
            items: result
                .items
                .into_iter()
                .filter(|entry| match &entry.change {
                    HistoryChange::EdgeAdded { other, .. }
                    | HistoryChange::EdgeRemoved { other, .. } => {
                        role_mask.contains_node(&*g, other)
                    }
                    _ => true,
                })
                .collect(),
        };
        return json_ok(node_history_json(&key, &visible));
    }
    // Full identity: no masking. Return 404 for absent keys (consistent with
    // GET /node/{key} and the Role branch above).
    let g = state.db.read();
    if !g.has_node(&key) {
        return key_not_found(key);
    }
    let result = match g.node_history(&key) {
        Ok(e) => e,
        Err(e) => return graph_err(e),
    };
    json_ok(node_history_json(&key, &result))
}

/// `GET /history/edge?a=&b=` — return the edge lifecycle between two nodes.
///
/// Response: `{ a, b, events: [{edge_type, commit, event, rule}], total_commits, horizon }`.
/// `horizon` is the oldest commit history still reaches; events before it were
/// pruned and are not in `events`.
/// Role tokens: BOTH `a` AND `b` must be visible in the role mask, otherwise
/// responds with 404 for the first invisible key (no existence oracle).
async fn edge_history_handler(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    let a = match qs.get("a").filter(|s| !s.is_empty()) {
        Some(s) => s.clone(),
        None => return err_response("missing query param a"),
    };
    let b = match qs.get("b").filter(|s| !s.is_empty()) {
        Some(s) => s.clone(),
        None => return err_response("missing query param b"),
    };
    if let AuthIdentity::Role(ref role_name) = identity {
        let g = state.db.read();
        let role_mask = match g.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        // BOTH endpoints must be visible (no oracle for either).
        if !role_mask.contains_node(&*g, &a) {
            return key_not_found(a);
        }
        if !role_mask.contains_node(&*g, &b) {
            return key_not_found(b);
        }
        let result = match g.edge_history(&a, &b) {
            Ok(r) => r,
            Err(e) => return graph_err(e),
        };
        return json_ok(edge_history_result_json(&a, &b, &result));
    }
    // Full identity: no masking.
    let g = state.db.read();
    let result = match g.edge_history(&a, &b) {
        Ok(r) => r,
        Err(e) => return graph_err(e),
    };
    json_ok(edge_history_result_json(&a, &b, &result))
}

/// `GET /history/was_linked?a=&b=&edge_type=&at_commit=` — point-in-time edge check.
///
/// Response: `{ a, b, edge_type, at_commit, linked }`.
/// Returns 400 (not 500) when `at_commit` is outside the visible horizon.
/// Role tokens: BOTH `a` AND `b` must be visible, same-as-absent otherwise.
async fn was_linked_handler(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    let a = match qs.get("a").filter(|s| !s.is_empty()) {
        Some(s) => s.clone(),
        None => return err_response("missing query param a"),
    };
    let b = match qs.get("b").filter(|s| !s.is_empty()) {
        Some(s) => s.clone(),
        None => return err_response("missing query param b"),
    };
    let edge_type = match qs.get("edge_type").filter(|s| !s.is_empty()) {
        Some(s) => s.clone(),
        None => return err_response("missing query param edge_type"),
    };
    // `at_commit` takes a 0-based frame index or an RFC 3339 date. A caller
    // asking "were they linked on 2026-06-19" should not have to find the frame
    // themselves — and a guessed index is how a plausible wrong answer is
    // returned.
    let at_commit: u64 = match qs.get("at_commit") {
        Some(s) => match s.parse::<u64>() {
            Ok(n) => n,
            Err(_) => match state.db.read().resolve_date(s) {
                Ok(n) => n,
                Err(e) => return graph_err(e),
            },
        },
        None => return err_response("missing query param at_commit"),
    };

    if let AuthIdentity::Role(ref role_name) = identity {
        let g = state.db.read();
        let role_mask = match g.mask_for_role(role_name) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        if !role_mask.contains_node(&*g, &a) {
            return key_not_found(a);
        }
        if !role_mask.contains_node(&*g, &b) {
            return key_not_found(b);
        }
        return match g.was_linked(&a, &b, &edge_type, at_commit) {
            Ok(linked) => json_ok(json!({
                "a": a, "b": b, "edge_type": edge_type,
                "at_commit": at_commit, "linked": linked,
            })),
            Err(e @ GraphError::CommitOutOfRange { .. }) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response(),
            Err(e) => graph_err(e),
        };
    }

    // Full identity: no masking.
    let g = state.db.read();
    match g.was_linked(&a, &b, &edge_type, at_commit) {
        Ok(linked) => json_ok(json!({
            "a": a, "b": b, "edge_type": edge_type,
            "at_commit": at_commit, "linked": linked,
        })),
        Err(e @ GraphError::CommitOutOfRange { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Err(e) => graph_err(e),
    }
}

/// `DELETE /node/{key}/prop/{field}` — remove a property via the group-commit queue.
async fn remove_node_prop(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Path((key, field)): Path<(String, String)>,
) -> Response {
    let db = state.db.clone();
    if let AuthIdentity::Role(role_name) = &identity {
        let role = role_name.clone();
        return match blocking_write(move || {
            db.submit_batch_authz(role, vec![BatchOp::RemoveProp { key, field }])
        })
        .await
        {
            Ok(_) => json_ok(json!({"ok": true})),
            Err(resp) => resp,
        };
    }
    match blocking_write(move || db.submit_batch(vec![BatchOp::RemoveProp { key, field }])).await {
        Ok(_) => json_ok(json!({"ok": true})),
        Err(resp) => resp,
    }
}

/// Constant-time byte-string equality, used for secret (token) comparison so
/// the match does not short-circuit on the first differing byte and leak the
/// token through response-timing. Length is compared up front (token length is
/// not treated as a secret); the byte loop is branch-free over equal lengths.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Maximum `depth` accepted by the neighborhood endpoint. Bounds the read-lock
/// hold time of a single BFS request on a dense graph (a hostile
/// `depth=4294967295` would otherwise traverse the whole graph under the read
/// guard, starving writers).
const MAX_NEIGHBORHOOD_DEPTH: u32 = 64;

/// Parse `POST /find_similar` `vector` — a non-empty array of numbers.
fn parse_find_similar_vector(v: Option<&Js>) -> Result<Vec<f64>, String> {
    let Some(arr) = v.and_then(Js::as_array) else {
        return Err("missing vector".into());
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        match item.as_f64() {
            Some(n) => out.push(n),
            None => return Err("vector must be an array of numbers".into()),
        }
    }
    if out.is_empty() {
        return Err("vector must be a non-empty array of numbers".into());
    }
    Ok(out)
}

/// Parse an optional JSON `mask` array of node keys.
fn parse_mask_keys(v: Option<&Js>) -> Result<Option<Vec<String>>, String> {
    match v {
        None | Some(Js::Null) => Ok(None),
        Some(Js::Array(arr)) => {
            let mut keys = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => keys.push(s.to_string()),
                    None => return Err("mask must be an array of strings".into()),
                }
            }
            Ok(Some(keys))
        }
        Some(_) => Err("mask must be an array of strings".into()),
    }
}

/// Parse optional `where` as a `PropPredicate`. Invalid predicates are QueryError.
fn parse_where_body(v: Option<&Js>) -> Result<Option<PropPredicate>, String> {
    match v {
        None | Some(Js::Null) => Ok(None),
        Some(w) => {
            let pred: PropPredicate =
                serde_json::from_value(w.clone()).map_err(|e| format!("where: {e}"))?;
            pred.validate_named("where")?;
            Ok(Some(pred))
        }
    }
}

/// Parse and bound the neighborhood `depth` query parameter.
fn resolve_neighborhood_depth(raw: Option<&str>) -> Result<u32, String> {
    match raw {
        None => Ok(1),
        Some(s) => {
            let d: u32 = s
                .parse()
                .map_err(|_| "depth must be an integer".to_string())?;
            if d > MAX_NEIGHBORHOOD_DEPTH {
                return Err(format!("depth must be ≤ {MAX_NEIGHBORHOOD_DEPTH}"));
            }
            Ok(d)
        }
    }
}

/// Confine a client-supplied backup `dest` to `root`, closing the arbitrary
/// filesystem-write vector (a full-access token could otherwise direct the
/// backup writer at `/etc/...`, `/root/.ssh/...`, etc.).
///
/// Rules: reject empty; reject any `..` segment; relative paths resolve under
/// `root`; absolute paths must already fall within `root`. The check is lexical
/// (no filesystem access) and safe because `..` is rejected before joining.
fn confine_backup_dest(dest: &str, root: &std::path::Path) -> Result<PathBuf, String> {
    if dest.is_empty() {
        return Err("missing or empty \"dest\" field".into());
    }
    let dest_path = std::path::Path::new(dest);
    if dest_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("backup \"dest\" may not contain \"..\" path segments".into());
    }
    let joined = if dest_path.is_absolute() {
        dest_path.to_path_buf()
    } else {
        root.join(dest_path)
    };
    if !joined.starts_with(root) {
        return Err(format!(
            "backup \"dest\" must be within the backup root ({}); \
             set MUSHROOMDB_BACKUP_DIR to change it",
            root.display()
        ));
    }
    Ok(joined)
}

/// Directory backups are confined to: `MUSHROOMDB_BACKUP_DIR` if set, else the
/// server's current working directory.
fn backup_root() -> PathBuf {
    std::env::var_os("MUSHROOMDB_BACKUP_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `POST /backup` — take a consistent backup of the database to `dest`.
///
/// The read guard is held for the duration of the file copies, which is the
/// correct cross-process synchronisation point: the server is the single
/// process touching the files, so holding the read lock excludes concurrent
/// in-process writers.  This is the safe alternative to running the
/// `mushroomdb backup` CLI against a live-served store.
///
/// Request body: `{"dest": "backup-dir"}`. `dest` is confined to the backup
/// root (`MUSHROOMDB_BACKUP_DIR`, else the server's working directory):
/// relative paths resolve under it, absolute paths must fall within it, and
/// `..` segments are rejected. This prevents a full-access token from writing
/// to arbitrary filesystem paths.
///
/// Responses:
/// - `200 OK` — backup completed; body is a `BackupReport` JSON object.
/// - `500 Internal Server Error` — backup succeeded but verification failed;
///   body is the `BackupReport` JSON object (examine `files` and `bytes`).
/// - `400 Bad Request` — missing or invalid `dest`.
/// - `403 Forbidden` — role-bound token; this endpoint requires a full-access token.
async fn backup(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
    Json(body): Json<Js>,
) -> Response {
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /backup requires a full-access token");
    }
    if let Some(refusal) = refuse_write_tier(&identity) {
        return refusal;
    }
    let root = backup_root();
    let dest = match body.get("dest").and_then(Js::as_str) {
        Some(s) => match confine_backup_dest(s, &root) {
            Ok(p) => p,
            Err(e) => return err_response(e),
        },
        None => return err_response("missing or empty \"dest\" field"),
    };
    let db = state.db.clone();
    let report: BackupReport = match tokio::task::spawn_blocking(move || {
        // The read guard is held inside spawn_blocking so file copies happen
        // with the write lock excluded.  The guard drops at end of closure.
        let g = db.read();
        g.backup_to(&dest)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return graph_err(e),
        Err(_) => return err_response("backup task panicked"),
    };

    let body = match serde_json::to_value(BackupReportJson::from(&report)) {
        Ok(v) => v,
        Err(e) => return err_response(e.to_string()),
    };

    if report.verified {
        json_ok(body)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

/// JSON-serialisable projection of [`BackupReport`].
#[derive(serde::Serialize)]
struct BackupReportJson<'a> {
    files: &'a [String],
    bytes: u64,
    verified: bool,
}

impl<'a> From<&'a BackupReport> for BackupReportJson<'a> {
    fn from(r: &'a BackupReport) -> Self {
        Self {
            files: &r.files,
            bytes: r.bytes,
            verified: r.verified,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::result_set_json;
    use mushroomdb::{DegreeConfig, PageRankConfig, ResultSet, Value, WccConfig};

    #[tokio::test]
    async fn an_unwritable_directory_is_a_500_with_a_short_client_message() {
        let resp = graph_err(GraphError::Unwritable {
            path: "/secret/dir/LOCK".into(),
            detail: "uid 65532 owner 0 /secret/dir".into(),
            read_only_filesystem: false,
        });
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains(GraphError::UNWRITABLE_CLIENT_MESSAGE),
            "{text}"
        );
        assert!(
            !text.contains("secret") && !text.contains("65532"),
            "{text}"
        );
    }

    #[test]
    fn nan_float_cell_serializes_as_null() {
        let mut rs = ResultSet::new(vec!["n".into()]);
        rs.push_row(vec![Some(Value::Float(f64::NAN))]);
        let j = result_set_json(&rs);
        assert_eq!(j["rows"][0][0], Js::Null);
    }

    /// Verify that `POST /algo/pagerank` accepts an empty JSON body `{}` and
    /// applies server defaults (regression guard for `#[serde(default)]`).
    #[test]
    fn pagerank_config_empty_body_uses_defaults() {
        let config: PageRankConfig = serde_json::from_str("{}").unwrap();
        let default = PageRankConfig::default();
        assert_eq!(config.damping, default.damping);
        assert_eq!(config.max_iters, default.max_iters);
        assert_eq!(config.tol, default.tol);
        assert_eq!(config.budget_ms, default.budget_ms);
        assert_eq!(config.edge_type, default.edge_type);
    }

    /// Same guard for `POST /algo/wcc`.
    #[test]
    fn wcc_config_empty_body_uses_defaults() {
        let config: WccConfig = serde_json::from_str("{}").unwrap();
        let default = WccConfig::default();
        assert_eq!(config.budget_ms, default.budget_ms);
        assert_eq!(config.edge_type, default.edge_type);
    }

    /// Same guard for `POST /algo/degree`.
    #[test]
    fn degree_config_empty_body_uses_defaults() {
        let config: DegreeConfig = serde_json::from_str("{}").unwrap();
        let default = DegreeConfig::default();
        assert_eq!(config.budget_ms, default.budget_ms);
        assert_eq!(config.edge_type, default.edge_type);
    }

    #[test]
    fn backup_dest_rejects_empty() {
        assert!(confine_backup_dest("", std::path::Path::new("/srv/backups")).is_err());
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret-token", b"secret-toke")); // shorter
        assert!(!constant_time_eq(b"secret-token", b"secret-tokex")); // last byte differs
        assert!(!constant_time_eq(b"secret-token", b"Xecret-token")); // first byte differs
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn depth_defaults_to_one_when_absent() {
        assert_eq!(resolve_neighborhood_depth(None).unwrap(), 1);
    }

    #[test]
    fn depth_within_cap_is_accepted() {
        assert_eq!(resolve_neighborhood_depth(Some("10")).unwrap(), 10);
        assert_eq!(
            resolve_neighborhood_depth(Some(&MAX_NEIGHBORHOOD_DEPTH.to_string())).unwrap(),
            MAX_NEIGHBORHOOD_DEPTH
        );
    }

    #[test]
    fn depth_over_cap_is_rejected() {
        assert!(resolve_neighborhood_depth(Some("65")).is_err());
        assert!(resolve_neighborhood_depth(Some("4294967295")).is_err());
    }

    #[test]
    fn depth_non_integer_is_rejected() {
        assert!(resolve_neighborhood_depth(Some("abc")).is_err());
    }

    #[test]
    fn backup_dest_rejects_parent_traversal() {
        let root = std::path::Path::new("/srv/backups");
        assert!(confine_backup_dest("../../etc/cron.d", root).is_err());
        assert!(confine_backup_dest("ok/../../../etc", root).is_err());
    }

    #[test]
    fn backup_dest_rejects_absolute_outside_root() {
        let root = std::path::Path::new("/srv/backups");
        assert!(confine_backup_dest("/etc/cron.d", root).is_err());
        assert!(confine_backup_dest("/root/.ssh/authorized_keys", root).is_err());
    }

    #[test]
    fn backup_dest_allows_relative_within_root() {
        let root = std::path::Path::new("/srv/backups");
        assert_eq!(
            confine_backup_dest("nightly", root).unwrap(),
            PathBuf::from("/srv/backups/nightly")
        );
        assert_eq!(
            confine_backup_dest("2026/aug", root).unwrap(),
            PathBuf::from("/srv/backups/2026/aug")
        );
    }

    #[test]
    fn backup_dest_allows_absolute_within_root() {
        let root = std::path::Path::new("/srv/backups");
        assert_eq!(
            confine_backup_dest("/srv/backups/x", root).unwrap(),
            PathBuf::from("/srv/backups/x")
        );
    }
}

#[cfg(test)]
mod read_parity_tests {
    //! TX-5 differential: every HTTP read route that moved from the read lock to
    //! the lock-free reader answers byte-for-byte what the read-lock path answered.
    //!
    //! The `legacy_*` functions are the pre-TX-5 handler bodies, kept here as the
    //! oracle: each runs under `db.read()` against the `GraphDb` handle, exactly as
    //! the route did before it moved. Every case sends the same request through the
    //! live router and compares status, content type and body bytes.
    //!
    //! The matrix runs over the house-rule axes: one batch vs separate commits, a
    //! never-closed handle with a delta tail, a snapshotted store with an overlay
    //! on top, and a reopened store (WAL replay).

    use super::*;
    use crate::json::{masked_edges_json, node_edges_json, node_info_json, result_set_json};
    use axum::body::{to_bytes, Body};
    use mushroomdb::{Predicate, RoleDef, RuleDef};
    use tower::ServiceExt;

    // ── The oracle: the read-lock bodies as they were before TX-5 ────────────────

    fn legacy_client_mask(
        db: &mushroomdb::GraphDb<mushroomdb::RealFs>,
        mask_param: &str,
        stub: bool,
    ) -> NodeMask {
        let keys = mask_param
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let m = NodeMask::from_keys(db, keys);
        if stub {
            m.with_mode(MaskMode::Stub)
        } else {
            m
        }
    }

    fn legacy_node_info(db: &SharedDb, key: &str, mask: &str, stub: bool) -> Response {
        let g = db.read();
        if !mask.is_empty() {
            let mask = legacy_client_mask(&g, mask, stub);
            return match g.node_info_masked(key, &mask) {
                Some(mushroomdb::MaskedNodeResult::Visible(info)) => json_ok(node_info_json(&info)),
                Some(mushroomdb::MaskedNodeResult::Restricted) => {
                    json_ok(crate::json::stub_node_json(key))
                }
                None => key_not_found(key.to_string()),
            };
        }
        match g.node_info(key) {
            Some(info) => json_ok(node_info_json(&info)),
            None => key_not_found(key.to_string()),
        }
    }

    fn legacy_node_edges(db: &SharedDb, key: &str, mask: &str, stub: bool) -> Response {
        let g = db.read();
        if !mask.is_empty() {
            let mask = legacy_client_mask(&g, mask, stub);
            return match g.node_edges_masked(key, &mask) {
                Ok(edges) => json_ok(masked_edges_json(&edges)),
                Err(GraphError::KeyNotFound { key }) => key_not_found(key),
                Err(e) => graph_err(e),
            };
        }
        match g.node_edges(key) {
            Ok(edges) => json_ok(node_edges_json(&edges)),
            Err(GraphError::KeyNotFound { key }) => key_not_found(key),
            Err(e) => graph_err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_neighborhood(
        db: &SharedDb,
        key: &str,
        depth: u32,
        edge_types: Option<&[&str]>,
        dir: Dir,
        mask: &str,
        stub: bool,
    ) -> Response {
        let g = db.read();
        if !mask.is_empty() {
            let mask = legacy_client_mask(&g, mask, stub);
            return match g.neighborhood_masked(key, depth, edge_types, dir, &mask) {
                Some(rs) => json_ok(result_set_json(&rs)),
                None => key_not_found(key.to_string()),
            };
        }
        match g.node_ref(key) {
            Some(n) => json_ok(result_set_json(&n.neighborhood(depth, edge_types, dir))),
            None => key_not_found(key.to_string()),
        }
    }

    /// The full-token, live `/query` read paths (unmasked, and client mask and/or
    /// namespace), after the request has been parsed.
    fn legacy_query(
        db: &SharedDb,
        cypher: &str,
        mask_keys: Option<&[String]>,
        namespace: Option<&str>,
        stub: bool,
        format: &str,
    ) -> Response {
        let params = BTreeMap::new();
        if mask_keys.is_some() || namespace.is_some() {
            let db = db.read();
            let m = match (mask_keys, namespace) {
                (Some(keys), Some(ns)) => {
                    NodeMask::from_keys(&*db, keys.iter().map(String::as_str))
                        .intersect(&db.mask_for_namespace(ns))
                }
                (Some(keys), None) => NodeMask::from_keys(&*db, keys.iter().map(String::as_str)),
                (None, Some(ns)) => db.mask_for_namespace(ns),
                (None, None) => unreachable!(),
            };
            let mask = if stub { m.with_mode(MaskMode::Stub) } else { m };
            return match db.query_masked(cypher, &params, &mask) {
                Ok(rs) => format_query_result(rs, format),
                Err(GraphError::MaskedReadOnly) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "masked queries are read-only"})),
                )
                    .into_response(),
                Err(e) => graph_err(e),
            };
        }
        // The route classifies the statement before it picks a lock; a statement
        // that does not lex is refused there, on either path.
        match is_write_query(cypher) {
            Ok(false) => {}
            Ok(true) => unreachable!("writes are not read routes"),
            Err(e) => return err_response(e),
        }
        match db.read().query(cypher, &params) {
            Ok(rs) => format_query_result(rs, format),
            Err(e) => graph_err(e),
        }
    }

    /// The role-token read of `/query`, resolved the read-lock way: the role's mask
    /// from the live handle, intersected as the route intersects it, and the query
    /// run by `GraphDb::query_masked`. The route itself has been on the reader since
    /// 0.6; this pins that its answer, `UNION` included, is the write handle's.
    fn legacy_role_query(
        db: &SharedDb,
        role: &str,
        cypher: &str,
        mask_keys: Option<&[String]>,
        namespace: Option<&str>,
    ) -> Response {
        // The role branch's own checks, which run before any lock on either path.
        match is_write_query(cypher) {
            Ok(false) => {}
            Ok(true) => unreachable!("writes are not read routes"),
            Err(e) => return err_response(e),
        }
        let g = db.read();
        let role_mask = match g.mask_for_role(role) {
            Ok(m) => m,
            Err(e) => return role_mask_err(e),
        };
        let mask = match mask_keys {
            Some(keys) => {
                role_mask.intersect(&NodeMask::from_keys(&*g, keys.iter().map(String::as_str)))
            }
            None => role_mask,
        };
        let mask = match namespace {
            Some(ns) => mask.intersect(&g.mask_for_namespace(ns)),
            None => mask,
        };
        match g.query_masked(cypher, &BTreeMap::new(), &mask) {
            Ok(rs) => format_query_result(rs, "json"),
            Err(GraphError::MaskedReadOnly) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "masked queries are read-only"})),
            )
                .into_response(),
            Err(e) => graph_err(e),
        }
    }

    // ── Plumbing ──────────────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct Answer {
        status: StatusCode,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    async fn answer(res: Response) -> Answer {
        let status = res.status();
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Answer {
            status,
            content_type,
            body,
        }
    }

    async fn routed(app: &Router, req: axum::http::Request<Body>) -> Answer {
        answer(app.clone().oneshot(req).await.unwrap()).await
    }

    fn get(uri: &str, token: Option<&str>) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder().method("GET").uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    fn post(uri: &str, body: Js, token: Option<&str>) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d =
            std::env::temp_dir().join(format!("tx5-parity-{name}-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    const FULL: &str = "admin";
    const ROLE: &str = "role-tok";

    /// How a fixture reaches its final state.
    #[derive(Clone, Copy, Debug)]
    enum Build {
        /// Every write its own commit, on a handle that never closed: a long
        /// delta tail over the open-time fold.
        SeparateCommits,
        /// The same graph written as one batch.
        OneBatch,
        /// Snapshotted part-way, then more writes: an mmap base with an overlay
        /// and a tail on top.
        Snapshotted,
        /// Written, closed, and reopened: the state comes from WAL replay.
        Reopened,
        /// Equality indexes declared and folded, then the second half on top:
        /// indexed lookups answered from the fold's index plus the tail.
        Indexed,
    }

    fn rule() -> RuleDef {
        RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        }
    }

    /// A weighted rule: its `score` is written on each edge by the rule engine,
    /// with no WAL record, and refreshed in place when an endpoint changes.
    fn weighted_rule() -> RuleDef {
        RuleDef {
            name: "skill_fit".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::Overlap {
                field: "skills".into(),
                min: 0.3,
            },
            edge_type: "FITS".into(),
            weight_prop: Some("score".into()),
            ..rule()
        }
    }

    fn skills(list: &[&str]) -> Value {
        Value::List(list.iter().map(|s| Value::Str(s.to_string())).collect())
    }

    fn person(key: &str, org: &str, ns: &str) -> BatchOp {
        let mine: &[&str] = match key.len() % 3 {
            0 => &["rust", "sql"],
            1 => &["rust", "go", "sql"],
            _ => &["go"],
        };
        BatchOp::InsertNode {
            label: "Person".into(),
            key: key.into(),
            props: vec![
                ("name".into(), Value::Str(key.to_uppercase())),
                ("org_id".into(), Value::Str(org.into())),
                ("age".into(), Value::Int(key.len() as i64 * 7)),
                ("ns".into(), Value::Str(ns.into())),
                ("skills".into(), skills(mine)),
            ],
        }
    }

    fn edge(t: &str, s: &str, d: &str) -> BatchOp {
        BatchOp::InsertEdge {
            edge_type: t.into(),
            src_key: s.into(),
            dst_key: d.into(),
        }
    }

    /// The graph every fixture holds: two orgs, people in two namespaces, a
    /// rule-derived `WORKS_AT` per person, asserted `KNOWS` edges (each inside one
    /// namespace, as the store requires) including a self-loop and a 2-cycle, a
    /// property removed, and a node deleted.
    fn first_half() -> Vec<BatchOp> {
        vec![
            BatchOp::InsertNode {
                label: "Org".into(),
                key: "o1".into(),
                props: vec![
                    ("name".into(), Value::Str("Acme".into())),
                    ("ns".into(), Value::Str("tenant-a".into())),
                    ("skills".into(), skills(&["rust", "sql", "go"])),
                ],
            },
            BatchOp::InsertNode {
                label: "Org".into(),
                key: "o2".into(),
                props: vec![
                    ("ns".into(), Value::Str("tenant-b".into())),
                    ("skills".into(), skills(&["go", "sql"])),
                ],
            },
            person("alice", "o1", "tenant-a"),
            person("bob", "o1", "tenant-a"),
            person("carol", "o2", "tenant-b"),
        ]
    }

    fn second_half() -> Vec<BatchOp> {
        vec![
            person("dave", "o2", "tenant-b"),
            person("erin", "o1", "tenant-a"),
            edge("KNOWS", "alice", "bob"),
            edge("KNOWS", "erin", "bob"),
            edge("KNOWS", "bob", "alice"),
            edge("KNOWS", "carol", "dave"),
            edge("KNOWS", "dave", "dave"),
            edge("LIKES", "erin", "alice"),
            edge("LIKES", "alice", "o1"),
            edge("LIKES", "dave", "o2"),
            BatchOp::SetProp {
                key: "bob".into(),
                field: "age".into(),
                value: Value::Int(41),
            },
            // Moves bob's `FITS` score in place: no edge is derived or retracted.
            BatchOp::SetProp {
                key: "bob".into(),
                field: "skills".into(),
                value: skills(&["rust", "sql", "go"]),
            },
            BatchOp::RemoveProp {
                key: "carol".into(),
                field: "age".into(),
            },
            person("zed", "o1", "tenant-a"),
            edge("KNOWS", "zed", "alice"),
            BatchOp::DeleteNode { key: "zed".into() },
            // Moves an indexed value: the fold's index still says CAROL.
            BatchOp::SetProp {
                key: "carol".into(),
                field: "name".into(),
                value: Value::Str("CAROLINE".into()),
            },
        ]
    }

    fn write_ops(db: &SharedDb, ops: Vec<BatchOp>, one_batch: bool) {
        if one_batch {
            db.submit_batch(ops).unwrap();
        } else {
            for op in ops {
                db.submit_batch(vec![op]).unwrap();
            }
        }
    }

    fn roles() -> mushroomdb::schema::Schema {
        mushroomdb::schema::Schema {
            roles: vec![RoleDef {
                name: "analyst".into(),
                labels: vec!["Person".into()],
                keys: vec!["o1".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        }
    }

    fn fixture(name: &str, build: Build) -> (Router, SharedDb) {
        let dir = tmp(name);
        let db = SharedDb::open(&dir).unwrap();
        db.write().apply_schema(&roles()).unwrap();
        db.write().create_rule(rule()).unwrap();
        db.write().create_rule(weighted_rule()).unwrap();
        match build {
            Build::SeparateCommits => {
                write_ops(&db, first_half(), false);
                write_ops(&db, second_half(), false);
            }
            Build::OneBatch => {
                let mut ops = first_half();
                ops.extend(second_half());
                write_ops(&db, ops, true);
            }
            Build::Snapshotted => {
                write_ops(&db, first_half(), false);
                db.write().snapshot().unwrap();
                write_ops(&db, second_half(), false);
            }
            Build::Reopened => {
                write_ops(&db, first_half(), false);
                write_ops(&db, second_half(), false);
            }
            Build::Indexed => {
                db.write().enable_index("Person", "name").unwrap();
                db.write().enable_index("Org", "name").unwrap();
                write_ops(&db, first_half(), false);
                // Enough commits to fold, so the indexes and the first half are
                // in the fold and the second half is the tail.
                for i in 0..mushroomdb::FOLD_EVERY_K {
                    db.submit_batch(vec![BatchOp::InsertNode {
                        label: "Pad".into(),
                        key: format!("pad{i}"),
                        props: vec![],
                    }])
                    .unwrap();
                }
                write_ops(&db, second_half(), false);
                assert!(db.reader().queries_use_index(), "{name}: index unused");
            }
        }
        let db = match build {
            Build::Reopened => {
                drop(db);
                SharedDb::open(&dir).unwrap()
            }
            _ => db,
        };
        let app = router_with_role_tokens(
            db.clone(),
            Some(FULL.into()),
            HashMap::from([(ROLE.to_string(), "analyst".to_string())]),
        );
        (app, db)
    }

    const KEYS: &[&str] = &["alice", "bob", "carol", "dave", "o1", "o2", "zed", "nobody"];
    const MASKS: &[(&str, bool)] = &[
        ("", false),
        ("alice,carol,o1", false),
        ("alice,carol,o1", true),
        ("dave", true),
        ("nobody", false),
    ];

    async fn node_routes_match(app: &Router, db: &SharedDb, label: &str) -> usize {
        let mut cases = 0;
        for key in KEYS {
            for (mask, stub) in MASKS {
                let qs = if mask.is_empty() {
                    String::new()
                } else {
                    format!("?mask={mask}&stub_hidden={stub}")
                };
                let got = routed(app, get(&format!("/node/{key}{qs}"), Some(FULL))).await;
                let want = answer(legacy_node_info(db, key, mask, *stub)).await;
                assert_eq!(got, want, "{label}: GET /node/{key}{qs}");

                let got = routed(app, get(&format!("/node/{key}/edges{qs}"), Some(FULL))).await;
                let want = answer(legacy_node_edges(db, key, mask, *stub)).await;
                assert_eq!(got, want, "{label}: GET /node/{key}/edges{qs}");
                cases += 2;

                for (depth, dir, dir_name, types) in [
                    (1, Dir::Both, "both", None),
                    (2, Dir::Out, "out", None),
                    (3, Dir::In, "in", Some("KNOWS")),
                    (2, Dir::Both, "both", Some("WORKS_AT,LIKES")),
                ] {
                    let mut uri = format!("/node/{key}/neighborhood?depth={depth}&dir={dir_name}");
                    if let Some(t) = types {
                        uri.push_str(&format!("&edge_types={t}"));
                    }
                    if !mask.is_empty() {
                        uri.push_str(&format!("&mask={mask}&stub_hidden={stub}"));
                    }
                    let names: Option<Vec<&str>> = types.map(|t| t.split(',').collect());
                    let got = routed(app, get(&uri, Some(FULL))).await;
                    let want = answer(legacy_neighborhood(
                        db,
                        key,
                        depth,
                        names.as_deref(),
                        dir,
                        mask,
                        *stub,
                    ))
                    .await;
                    assert_eq!(got, want, "{label}: GET {uri}");
                    cases += 1;
                }
            }
        }
        cases
    }

    const CYPHERS: &[&str] = &[
        "MATCH (n:Person {name: 'BOB'}) RETURN n.key, n.age",
        "MATCH (n:Person) WHERE n.name = 'CAROLINE' RETURN n.key",
        "MATCH (n:Person {name: 'CAROL'}) RETURN n.key",
        "MATCH (n:Person {name: 'ZED'}) RETURN n.key",
        "MATCH (n:Person {name: 'ERIN'}) RETURN n.key",
        "MATCH (o:Org {name: 'Acme'})<-[:WORKS_AT]-(p) RETURN p.key",
        "MATCH (n:Person) RETURN n.name ORDER BY n.name",
        "MATCH (n:Person)-[:KNOWS]->(m) RETURN n.name, m.name ORDER BY n.name, m.name",
        "MATCH (n:Person)-[:WORKS_AT]->(o:Org) RETURN n.name, o.name",
        "MATCH (n:Person)-[r:FITS]->(o:Org) RETURN n.name, o.name, r.score ORDER BY n.name, o.name",
        "MATCH (n:Person) WHERE n.age > 30 RETURN n.name, n.age",
        "MATCH (n:Person) RETURN count(n) AS c",
        "MATCH (n:Person) RETURN n.ns AS ns UNION MATCH (o:Org) RETURN o.ns AS ns",
        "MATCH (n:Person) RETURN n.org_id AS x UNION ALL MATCH (n:Person) RETURN n.org_id AS x",
        "MATCH (n:Person) RETURN n.name UNION MATCH (o:Org) RETURN o.key",
        "MATCH (n:Person RETURN n",
        "MATCH (n:Person) RETURN",
        "MATCH (n:Person) RETURN n.name UNION",
        "MATCH (n) RETURN nosuchfn(n.name)",
        "MATCH (n:Person) RETURN m.name",
        "MATCH (n) RETURN \u{1}",
        "MATCH (n:Person {key: 'zed'}) RETURN n",
        "CREATE (x:Person {key: 'mallory'})",
    ];

    async fn query_routes_match(app: &Router, db: &SharedDb, label: &str) -> usize {
        let mut cases = 0;
        let masks: [Option<Vec<String>>; 3] = [
            None,
            Some(vec!["alice".into(), "carol".into(), "o2".into()]),
            Some(vec!["nobody".into()]),
        ];
        for cypher in CYPHERS {
            for mask in &masks {
                for namespace in [None, Some("tenant-a"), Some("no-such-ns")] {
                    for stub in [false, true] {
                        // A write without a mask or namespace is not a read route.
                        if cypher.starts_with("CREATE") && mask.is_none() && namespace.is_none() {
                            continue;
                        }
                        if stub && mask.is_none() && namespace.is_none() {
                            continue;
                        }
                        for format in ["json", ""] {
                            let mut body = json!({ "cypher": cypher });
                            if let Some(m) = mask {
                                body["mask"] = json!(m);
                            }
                            if let Some(ns) = namespace {
                                body["namespace"] = json!(ns);
                            }
                            if stub {
                                body["stub_hidden"] = json!(true);
                            }
                            let uri = if format.is_empty() {
                                "/query".to_string()
                            } else {
                                format!("/query?format={format}")
                            };
                            let got = routed(app, post(&uri, body.clone(), Some(FULL))).await;
                            let want = answer(legacy_query(
                                db,
                                cypher,
                                mask.as_deref(),
                                namespace,
                                stub,
                                format,
                            ))
                            .await;
                            assert_eq!(got, want, "{label}: POST {uri} {body}");
                            cases += 1;
                        }
                    }
                }
            }
        }
        cases
    }

    async fn role_queries_match(app: &Router, db: &SharedDb, label: &str) -> usize {
        let mut cases = 0;
        let masks: [Option<Vec<String>>; 2] = [None, Some(vec!["alice".into(), "o1".into()])];
        for cypher in CYPHERS.iter().filter(|c| !c.starts_with("CREATE")) {
            for mask in &masks {
                for namespace in [None, Some("tenant-a")] {
                    let mut body = json!({ "cypher": cypher });
                    if let Some(m) = mask {
                        body["mask"] = json!(m);
                    }
                    if let Some(ns) = namespace {
                        body["namespace"] = json!(ns);
                    }
                    let got =
                        routed(app, post("/query?format=json", body.clone(), Some(ROLE))).await;
                    let want = answer(legacy_role_query(
                        db,
                        "analyst",
                        cypher,
                        mask.as_deref(),
                        namespace,
                    ))
                    .await;
                    assert_eq!(got, want, "{label}: role POST /query {body}");
                    cases += 1;
                }
            }
        }
        cases
    }

    async fn all_routes_match(build: Build, name: &str) {
        let (app, db) = fixture(name, build);
        // The fixture must exercise what it claims to: a tail the reader applies.
        if !matches!(build, Build::Reopened) {
            assert!(
                !db.read().reader().deltas.is_empty(),
                "{name}: no delta tail"
            );
        }
        let label = format!("{build:?}");
        let n = node_routes_match(&app, &db, &label).await
            + query_routes_match(&app, &db, &label).await
            + role_queries_match(&app, &db, &label).await
            + counter_and_algo_routes_match(&app, &db, &label).await;
        assert!(n > 500, "{label}: only {n} cases ran");
    }

    // ── /health, /metrics and /algo/* (moved in fix round 1) ──────────────────

    fn legacy_health(db: &SharedDb) -> Response {
        let (nodes, edges) = {
            let g = db.read();
            let s = g.stats();
            (s.nodes_live, s.edges)
        };
        json_ok(json!({
            "ok": true,
            "nodes": nodes,
            "edges": edges,
            "addr": default_advertise_addr().to_string(),
        }))
    }

    /// `/metrics` before TX-5, minus `rss_bytes` and `uptime_s`, which move
    /// between any two calls.
    fn legacy_metrics(db: &SharedDb) -> Js {
        let g = db.read();
        let s = g.stats();
        let slow = g.slow_query_snapshot();
        json!({
            "nodes_live": s.nodes_live,
            "nodes_tombstoned": s.nodes_tombstoned,
            "edges": s.edges,
            "commit_seq": g.commit_seq(),
            "wal_size_bytes": g.wal_size_bytes().ok(),
            "slow_queries": {
                "threshold_ms": slow.threshold_ms,
                "count": slow.count,
                "last": slow.last.iter().map(|e| json!({
                    "ms": e.ms, "query": e.query, "at_commit": e.at_commit,
                })).collect::<Vec<_>>(),
            },
        })
    }

    fn legacy_algo(db: &SharedDb, route: &str, body: &Js) -> Response {
        let g = db.read();
        let v = match route {
            "pagerank" => {
                serde_json::to_value(g.pagerank(&serde_json::from_value(body.clone()).unwrap()))
            }
            "wcc" => serde_json::to_value(
                g.connected_components(&serde_json::from_value(body.clone()).unwrap()),
            ),
            _ => serde_json::to_value(
                g.degree_centrality(&serde_json::from_value(body.clone()).unwrap()),
            ),
        };
        json_ok(v.unwrap_or_else(|_| json!({})))
    }

    async fn counter_and_algo_routes_match(app: &Router, db: &SharedDb, label: &str) -> usize {
        let mut cases = 0;
        let got = routed(app, get("/health", Some(FULL))).await;
        assert_eq!(got, answer(legacy_health(db)).await, "{label}: /health");
        let got = routed(app, get("/health", None)).await;
        assert_eq!(got.body, br#"{"ok":true}"#, "{label}: anonymous /health");

        let got = routed(app, get("/metrics", Some(FULL))).await;
        assert_eq!(got.status, StatusCode::OK);
        let mut v: Js = serde_json::from_slice(&got.body).unwrap();
        let obj = v.as_object_mut().unwrap();
        assert!(obj.remove("rss_bytes").is_some() && obj.remove("uptime_s").is_some());
        assert_eq!(v, legacy_metrics(db), "{label}: /metrics");
        cases += 3;

        for route in ["pagerank", "wcc", "degree"] {
            for body in [
                json!({}),
                json!({"edge_type": "KNOWS"}),
                json!({"edge_type": "FITS"}),
            ] {
                let got = routed(
                    app,
                    post(&format!("/algo/{route}"), body.clone(), Some(FULL)),
                )
                .await;
                let want = answer(legacy_algo(db, route, &body)).await;
                assert_eq!(got, want, "{label}: /algo/{route} {body}");
                cases += 1;
            }
        }
        cases
    }

    #[tokio::test]
    async fn reader_routes_match_the_read_lock_separate_commits() {
        all_routes_match(Build::SeparateCommits, "separate").await;
    }

    #[tokio::test]
    async fn reader_routes_match_the_read_lock_one_batch() {
        all_routes_match(Build::OneBatch, "batch").await;
    }

    #[tokio::test]
    async fn reader_routes_match_the_read_lock_snapshotted() {
        all_routes_match(Build::Snapshotted, "snapshotted").await;
    }

    #[tokio::test]
    async fn reader_routes_match_the_read_lock_reopened() {
        all_routes_match(Build::Reopened, "reopened").await;
    }

    /// The cases are only a differential if the two sides can disagree: `UNION` on
    /// the role path, which the reader refused before TX-5, now answers rows.
    #[tokio::test]
    async fn union_answers_on_the_reader_for_both_identities() {
        let (app, _db) = fixture("union", Build::SeparateCommits);
        let cypher = "MATCH (n:Person) RETURN n.ns AS ns UNION MATCH (o:Org) RETURN o.ns AS ns";
        for token in [FULL, ROLE] {
            let got = routed(
                &app,
                post(
                    "/query?format=json",
                    json!({ "cypher": cypher }),
                    Some(token),
                ),
            )
            .await;
            assert_eq!(got.status, StatusCode::OK, "{token}: {:?}", got.body);
            let v: Js = serde_json::from_slice(&got.body).unwrap();
            assert!(
                !v["rows"].as_array().unwrap().is_empty(),
                "{token}: UNION returned no rows: {v}"
            );
        }
    }

    /// A store whose indexes are in the fold, with a tail that moves, deletes
    /// and adds indexed values: the reader serves the lookups from the fold's
    /// index layered with the tail and answers what the read lock answered.
    #[tokio::test]
    async fn reader_routes_match_the_read_lock_indexed() {
        all_routes_match(Build::Indexed, "indexed").await;
    }

    /// An unmasked reader query still lands in the `/metrics` slow-query ring.
    #[tokio::test]
    async fn a_slow_reader_query_is_still_recorded() {
        let (app, db) = fixture("slow", Build::SeparateCommits);
        let bulk = (0..5_000)
            .map(|i| BatchOp::InsertNode {
                label: "Bulk".into(),
                key: format!("bulk{i}"),
                props: vec![("n".into(), Value::Int(i))],
            })
            .collect();
        db.submit_batch(bulk).unwrap();
        let before = db.read().slow_query_snapshot().count;
        // Threshold 1 ms: a filtered scan of 5 000 nodes reaches it, and the count
        // is what is asserted, not the duration.
        db.write().set_slow_query_threshold_ms(1);
        let cypher = "MATCH (n:Bulk) WHERE n.n >= 0 RETURN count(n) AS c";
        let mut recorded = false;
        for _ in 0..20 {
            let got = routed(
                &app,
                post(
                    "/query?format=json",
                    json!({ "cypher": cypher }),
                    Some(FULL),
                ),
            )
            .await;
            assert_eq!(got.status, StatusCode::OK);
            if db.read().slow_query_snapshot().count > before {
                recorded = true;
                break;
            }
        }
        assert!(recorded, "a slow reader query never reached the ring");
        let last = db.read().slow_query_snapshot().last;
        assert_eq!(last.last().map(|e| e.query.as_str()), Some(cypher));
    }
}
