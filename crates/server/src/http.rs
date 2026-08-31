//! Thin HTTP wrapper over [`SharedDb`]. Every endpoint is a lock, a public
//! core-api call, then a response — no business logic.
//!
//! # Single-sink design
//!
//! The HTTP router is the designated broadcast producer. [`router`] installs
//! one `broadcast::Sender` as the [`core_api::GraphDb`] event sink. MCP and
//! CLI mutations on the same [`SharedDb`] fire into that same sink (one
//! producer, many `/watch` subscribers). A second [`router`] call replaces
//! the sink and terminates every existing subscriber with
//! [`tokio::sync::broadcast::error::RecvError::Closed`].

use crate::json::{
    edge_history_result_json, node_edges_json, node_history_json, node_info_json, params_from_json,
    parse_ingest_edges, result_set_json, rule_def_from_json,
};
use crate::{AppState, AuthIdentity};
use arrow_bridge::to_ipc_bytes;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_api::{
    is_write_query, json_to_rows, json_to_value, AutoFk, BackupReport, BatchOp, DegreeConfig, Dir,
    GraphError, IngestOptions, MaskMode, NodeMask, PageRankConfig, ResultSet, SharedDb,
    SuggestConfig, Value, WccConfig, SUGGEST_DEFAULT_SEED,
};
use serde_json::{json, Value as Js};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::services::ServeDir;

/// Build the HTTP router over `db`. Read endpoints take the read lock;
/// `/ingest` takes the write lock. Guards are dropped before any `.await`.
/// `GET /watch` upgrades to a WebSocket fed by the post-commit sink.
///
/// **Call at most once per [`SharedDb`].** A second call replaces the sink
/// and terminates all existing `/watch` subscribers with
/// [`tokio::sync::broadcast::error::RecvError::Closed`].
///
/// Installing the watch sink replaces any previously installed
/// [`core_api::GraphDb::set_event_sink`]. The sink only
/// `broadcast::Sender::send`s (non-blocking) and never re-enters `db`.
///
/// # Blocking
///
/// [`SharedDb`] uses a std [`std::sync::RwLock`]. Write handlers
/// (`query_write`, `/ingest`, `create_rule`) run in
/// `tokio::task::spawn_blocking` and drop the write guard before `.await`.
/// Reads stay on the worker (neighborhood is µs). `suggest` and `algo`
/// already use the blocking pool.
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
    serve_inner(db, addr, ready, UiFallback::None, token, HashMap::new()).await
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
    serve_inner(db, addr, ready, UiFallback::None, token, role_tokens).await
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
    serve_inner(db, addr, ready, UiFallback::Dir(ui_dir), token, role_tokens).await
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
    serve_inner(db, addr, ready, UiFallback::Embedded, token, role_tokens).await
}

enum UiFallback {
    None,
    Dir(PathBuf),
    #[cfg(feature = "embed-ui")]
    Embedded,
}

async fn serve_inner(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui: UiFallback,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        // Caller dropped the readiness receiver; still serve.
        eprintln!("serve: readiness receiver dropped before bind notify");
    }
    let app = build_app(db, token, role_tokens, ui, local);
    axum::serve(listener, app).await
}

fn default_advertise_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn build_app(
    db: SharedDb,
    token: Option<String>,
    role_tokens: HashMap<String, String>,
    ui: UiFallback,
    addr: SocketAddr,
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
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/query", post(query))
        .route("/stats", get(stats))
        .route("/ingest", post(ingest))
        .route("/rules", post(create_rule))
        .route("/suggest", get(suggest))
        .route("/explain", get(explain))
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
        .route("/watch", get(crate::ws::watch))
        .route("/subscribe", get(crate::subscribe::subscribe))
        .with_state(state.clone());
    let app = match ui {
        UiFallback::None => app,
        UiFallback::Dir(dir) => app.fallback_service(ServeDir::new(dir)),
        #[cfg(feature = "embed-ui")]
        UiFallback::Embedded => app.fallback(embedded_fallback),
    };
    app.layer(middleware::from_fn_with_state(state, auth_middleware))
        // Cap request bodies (default axum limit is only 2 MiB; we allow larger
        // ingest batches but reject multi-GB bodies that would OOM the collector
        // before any handler runs).
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
}

/// Maximum accepted HTTP request body size (64 MiB). Large enough for batched
/// `/ingest` payloads, small enough to prevent a single request from
/// exhausting memory. Clients with larger imports should chunk.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

async fn health(State(state): State<AppState>) -> Response {
    let (nodes, edges) = {
        let g = state.db.read();
        let s = g.stats();
        (s.nodes_live, s.edges)
    };
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
    F: FnOnce() -> core_api::Result<T> + Send + 'static,
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
    if state.token.is_none() && state.role_tokens.is_empty() {
        req.extensions_mut().insert(AuthIdentity::Full);
        return next.run(req).await;
    }

    // Health is always open regardless of token configuration.
    if req.method() == Method::GET && req.uri().path() == "/health" {
        req.extensions_mut().insert(AuthIdentity::Full);
        return next.run(req).await;
    }

    let presented = request_token(&req);

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
                attach_token_cookie(&mut res, full_tok);
            }
            return res;
        }
    }

    // Check role-bound tokens.
    if let Some(tok) = presented.as_deref() {
        if let Some(role_name) = state.role_tokens.get(tok) {
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

    // Nothing matched → 401.
    unauthorized()
}

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

fn attach_token_cookie(res: &mut Response, token: &str) {
    let value = format!("{TOKEN_COOKIE}={token}; Path=/; SameSite=Lax; HttpOnly");
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

fn graph_err(e: GraphError) -> Response {
    match e {
        // §4.3: role-scoped write denials map to 403 with the verbatim reason
        // string (Display delegates to reason, so .to_string() == reason).
        GraphError::RoleWriteDenied { reason } => forbidden(&reason),
        GraphError::QueryError { detail } | GraphError::IngestError { detail } => {
            err_response(detail)
        }
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

    // Role token: write Cypher routes to query_write_authz (scope + mask
    // resolved under the write lock, §5 discipline).  Read Cypher uses the
    // lock-free epoch snapshot path unchanged.
    if let AuthIdentity::Role(ref role_name) = identity {
        let is_write = match is_write_query(&cypher) {
            Ok(b) => b,
            Err(e) => return err_response(e),
        };
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
    // Hold a single read guard for both from_keys and query_masked so the mask
    // and the query execute on the same database snapshot.
    //
    // `stub_hidden: true` opts into MaskMode::Stub for the mask; Cypher query
    // behaviour is identical in both modes (hidden nodes are excluded from
    // query results regardless of mode).
    if let Some(ref keys) = mask_keys {
        let stub_hidden = body
            .get("stub_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let db = state.db.read();
        let mask = {
            let m = NodeMask::from_keys(&*db, keys.iter().map(String::as_str));
            if stub_hidden {
                m.with_mode(MaskMode::Stub)
            } else {
                m
            }
        };
        return match db.query_masked(&cypher, &params, &mask) {
            Ok(rs) => format_query_result(rs, format),
            Err(GraphError::MaskedReadOnly) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "masked queries are read-only"})),
            )
                .into_response(),
            Err(e) => graph_err(e),
        };
    }

    // Detect write statements to dispatch to the correct lock.
    // Write statements (CREATE / MATCH…SET / MATCH…DELETE / MERGE) need the
    // write lock so mutations flow through WAL + rule engine with fsync before
    // the response is sent.  Read queries (MATCH … RETURN …) use the read lock.
    let is_write = match is_write_query(&cypher) {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };

    let rs = if is_write {
        let db = state.db.clone();
        match blocking_write(move || db.write().query_write(&cypher, &params)).await {
            Ok(rs) => rs,
            Err(resp) => return resp,
        }
    } else {
        match state.db.read().query(&cypher, &params) {
            Ok(rs) => rs,
            Err(e) => return graph_err(e),
        }
    };

    format_query_result(rs, format)
}

async fn stats(
    State(state): State<AppState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Response {
    // v1: deny role tokens — raw counts leak graph size beyond the role's subgraph.
    if let AuthIdentity::Role(_) = identity {
        return forbidden("role-bound token: /stats requires a full-access token");
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
    match blocking_write(move || db.write().create_rule(def)).await {
        Ok(()) => json_ok(json!({"ok": true, "name": name})),
        Err(resp) => resp,
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
        let g = state.db.read();
        let mask = {
            let keys = mask_param
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let m = NodeMask::from_keys(&*g, keys);
            if stub_hidden {
                m.with_mode(MaskMode::Stub)
            } else {
                m
            }
        };
        return match g.node_info_masked(&key, &mask) {
            Some(core_api::MaskedNodeResult::Visible(info)) => json_ok(node_info_json(&info)),
            Some(core_api::MaskedNodeResult::Restricted) => {
                json_ok(crate::json::stub_node_json(&key))
            }
            None => key_not_found(key),
        };
    }

    let info = {
        let g = state.db.read();
        g.node_info(&key)
    };
    match info {
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
        return match snap.node_edges(&key) {
            Ok(edges) => {
                // Filter out edges whose OTHER endpoint is hidden in the role
                // mask.  A role token must not learn about hidden neighbors via
                // the edge list even when the entry key itself is visible.
                let visible: Vec<_> = edges
                    .into_iter()
                    .filter(|e| {
                        let other = if e.src_key == key {
                            &e.dst_key
                        } else {
                            &e.src_key
                        };
                        snap.resolve_key(other)
                            .is_some_and(|id| role_mask.contains_id(id))
                    })
                    .collect();
                json_ok(node_edges_json(&visible))
            }
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
        let g = state.db.read();
        let mask = {
            let keys = mask_param
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let m = NodeMask::from_keys(&*g, keys);
            if stub_hidden {
                m.with_mode(MaskMode::Stub)
            } else {
                m
            }
        };
        return match g.node_edges_masked(&key, &mask) {
            Ok(edges) => json_ok(crate::json::masked_edges_json(&edges)),
            Err(GraphError::KeyNotFound { key }) => key_not_found(key),
            Err(e) => graph_err(e),
        };
    }

    let out = {
        let g = state.db.read();
        g.node_edges(&key)
    };
    match out {
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
        if !snap
            .resolve_key(&key)
            .is_some_and(|id| role_mask.contains_id(id))
        {
            return key_not_found(key);
        }
        // Use the mask-aware BFS: hidden nodes are excluded from results AND
        // cannot be used as traversal intermediaries (never-leak invariant).
        let rs = match snap.neighborhood_masked(&key, depth, etype_refs.as_deref(), dir, &role_mask)
        {
            Some(rs) => rs,
            None => return key_not_found(key),
        };
        return json_ok(result_set_json(&rs));
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
        let g = state.db.read();
        let mask = {
            let keys = mask_param
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let m = NodeMask::from_keys(&*g, keys);
            if stub_hidden {
                m.with_mode(MaskMode::Stub)
            } else {
                m
            }
        };
        return match g.neighborhood_masked(&key, depth, etype_refs.as_deref(), dir, &mask) {
            Some(rs) => json_ok(result_set_json(&rs)),
            None => graph_err(GraphError::KeyNotFound { key: key.clone() }),
        };
    }

    // Unmasked full-token path (no mask param).
    let rs = {
        let g = state.db.read();
        match g.node_ref(&key) {
            Some(n) => Ok(n.neighborhood(depth, etype_refs.as_deref(), dir)),
            None => Err(GraphError::KeyNotFound { key: key.clone() }),
        }
    };
    match rs {
        Ok(rs) => json_ok(result_set_json(&rs)),
        Err(e) => graph_err(e),
    }
}

/// `POST /algo/pagerank` — run PageRank over the unified topology.
///
/// # Locking and blocking strategy
///
/// PageRank is CPU-intensive and synchronous. This handler offloads the work
/// to `tokio::task::spawn_blocking` (blocking thread-pool). The read guard is
/// acquired and held inside the blocking task — reads don't block other reads.
/// The `budget_ms` field in [`PageRankConfig`] caps lock-hold time.
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
    let db = state.db.clone();
    match tokio::task::spawn_blocking(move || db.read().pagerank(&config)).await {
        Ok(report) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Err(_) => err_response("pagerank task panicked"),
    }
}

/// `POST /algo/wcc` — weakly-connected components over the unified topology.
///
/// Mirrors the `suggest` locking and blocking pattern exactly: spawn_blocking,
/// read guard inside, `budget_ms` in config caps lock-hold time.
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
    match tokio::task::spawn_blocking(move || db.read().connected_components(&config)).await {
        Ok(report) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Err(_) => err_response("wcc task panicked"),
    }
}

/// `POST /algo/degree` — degree centrality over the unified topology.
///
/// Mirrors the `suggest` locking and blocking pattern exactly.
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
    match tokio::task::spawn_blocking(move || db.read().degree_centrality(&config)).await {
        Ok(report) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
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
        if let Some(v) = json_to_value(val.clone()) {
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
    let props = match body.get("props") {
        None | Some(Js::Null) => vec![],
        Some(v) => match props_from_json_obj(v) {
            Ok(p) => p,
            Err(e) => return err_response(e),
        },
    };
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
    let value = match body.get("value").and_then(|v| json_to_value(v.clone())) {
        Some(v) => v,
        None => {
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
/// Response: `{ key, history: [{commit, change}], total_commits }`.
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
        let entries = match g.node_history(&key) {
            Ok(e) => e,
            Err(e) => return graph_err(e),
        };
        let total_commits = match g.wal_total_commits() {
            Ok(n) => n,
            Err(e) => return graph_err(e),
        };
        // Filter EdgeAdded/EdgeRemoved entries whose `other` endpoint is hidden.
        // A role token must not learn about hidden nodes via edge history events —
        // mirrors the same protection in `node_edges` (http.rs ~978-989).
        use core_api::HistoryChange;
        let visible: Vec<_> = entries
            .into_iter()
            .filter(|entry| match &entry.change {
                HistoryChange::EdgeAdded { other, .. }
                | HistoryChange::EdgeRemoved { other, .. } => role_mask.contains_node(&*g, other),
                _ => true,
            })
            .collect();
        return json_ok(node_history_json(&key, &visible, total_commits));
    }
    // Full identity: no masking. Return 404 for absent keys (consistent with
    // GET /node/{key} and the Role branch above).
    let g = state.db.read();
    if !g.has_node(&key) {
        return key_not_found(key);
    }
    let entries = match g.node_history(&key) {
        Ok(e) => e,
        Err(e) => return graph_err(e),
    };
    let total_commits = match g.wal_total_commits() {
        Ok(n) => n,
        Err(e) => return graph_err(e),
    };
    json_ok(node_history_json(&key, &entries, total_commits))
}

/// `GET /history/edge?a=&b=` — return the edge lifecycle between two nodes.
///
/// Response: `{ a, b, events: [{edge_type, commit, event, rule}], total_commits }`.
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
    let at_commit: u64 = match qs.get("at_commit") {
        Some(s) => match s.parse() {
            Ok(n) => n,
            Err(_) => return err_response("at_commit must be a non-negative integer"),
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
            Err(GraphError::CommitOutOfRange { .. }) => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("commit {at_commit} is out of range")})),
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
        Err(GraphError::CommitOutOfRange { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("commit {at_commit} is out of range")})),
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
    use core_api::{DegreeConfig, PageRankConfig, ResultSet, Value, WccConfig};

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
