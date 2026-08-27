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
    node_edges_json, node_info_json, params_from_json, parse_ingest_edges, result_set_json,
    rule_def_from_json,
};
use crate::AppState;
use arrow_bridge::to_ipc_bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_api::{
    is_write_query, json_to_rows, AutoFk, DegreeConfig, Dir, GraphError, IngestOptions, NodeMask,
    PageRankConfig, SharedDb, SuggestConfig, WccConfig, SUGGEST_DEFAULT_SEED,
};
use serde_json::{json, Value as Js};
use std::collections::BTreeMap;
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
pub fn router_with_auth(db: SharedDb, token: Option<String>) -> Router {
    build_app(db, token, UiFallback::None, default_advertise_addr())
}

/// Same as [`router_with_auth`], then `ServeDir` as the fallback so API routes win.
pub fn router_with_ui(
    db: SharedDb,
    ui_dir: impl AsRef<std::path::Path>,
    token: Option<String>,
) -> Router {
    build_app(
        db,
        token,
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
    build_app(db, None, UiFallback::Embedded, default_advertise_addr())
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
pub async fn serve(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::None, token).await
}

/// [`serve`] plus a UI dist directory mounted behind the API routes.
pub async fn serve_with_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
    token: Option<String>,
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::Dir(ui_dir), token).await
}

/// [`serve`] plus the compiled-in UI (no-op fallback if `embed-ui` is off).
#[cfg(feature = "embed-ui")]
pub async fn serve_with_embedded_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    token: Option<String>,
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::Embedded, token).await
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
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        // Caller dropped the readiness receiver; still serve.
        eprintln!("serve: readiness receiver dropped before bind notify");
    }
    let app = build_app(db, token, ui, local);
    axum::serve(listener, app).await
}

fn default_advertise_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

fn build_app(db: SharedDb, token: Option<String>, ui: UiFallback, addr: SocketAddr) -> Router {
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
        .route("/node/{key}/edges", get(node_edges))
        .route("/node/{key}/neighborhood", get(neighborhood))
        .route("/algo/pagerank", post(algo_pagerank))
        .route("/algo/wcc", post(algo_wcc))
        .route("/algo/degree", post(algo_degree))
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
}

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

async fn auth_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.token.clone().filter(|s| !s.is_empty()) else {
        return next.run(req).await;
    };
    if req.method() == Method::GET && req.uri().path() == "/health" {
        return next.run(req).await;
    }
    if request_token(&req).as_deref() != Some(expected.as_str()) {
        return unauthorized();
    }
    let set_cookie = presented_bearer_or_query(&req).as_deref() == Some(expected.as_str());
    let mut res = next.run(req).await;
    if set_cookie && is_html_response(&res) {
        attach_token_cookie(&mut res, &expected);
    }
    res
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

fn err_response(detail: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": detail.into()})),
    )
        .into_response()
}

fn graph_err(e: GraphError) -> Response {
    let detail = match e {
        GraphError::QueryError { detail } | GraphError::IngestError { detail } => detail,
        other => other.to_string(),
    };
    err_response(detail)
}

fn key_not_found(key: String) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": GraphError::KeyNotFound { key }.to_string()})),
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

async fn query(
    State(state): State<AppState>,
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

    // Optional node mask: when present, route to query_masked (read-only).
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

    // When a mask is provided, route to query_masked (rejects writes).
    if let Some(ref keys) = mask_keys {
        let mask = NodeMask::from_keys(&*state.db.read(), keys.iter().map(String::as_str));
        return match state.db.read().query_masked(&cypher, &params, &mask) {
            Ok(rs) => match format {
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
            },
            Err(GraphError::QueryError { detail }) if detail.contains("masked queries are read-only") => {
                (StatusCode::BAD_REQUEST, Json(json!({"error": detail}))).into_response()
            }
            Err(e) => graph_err(e),
        };
    }

    // Detect write statements at the token level to dispatch to the correct lock.
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

async fn stats(State(state): State<AppState>) -> Response {
    let snap = {
        let g = state.db.read();
        g.stats()
    };
    match serde_json::to_value(&snap) {
        Ok(v) => json_ok(v),
        Err(e) => err_response(e.to_string()),
    }
}

async fn ingest(State(state): State<AppState>, Json(body): Json<Js>) -> Response {
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
async fn suggest(State(state): State<AppState>) -> Response {
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

async fn create_rule(State(state): State<AppState>, Json(body): Json<Js>) -> Response {
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
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
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

async fn node_info(State(state): State<AppState>, Path(key): Path<String>) -> Response {
    let info = {
        let g = state.db.read();
        g.node_info(&key)
    };
    match info {
        Some(info) => json_ok(node_info_json(&info)),
        None => key_not_found(key),
    }
}

async fn node_edges(State(state): State<AppState>, Path(key): Path<String>) -> Response {
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
    Path(key): Path<String>,
    Query(qs): Query<BTreeMap<String, String>>,
) -> Response {
    let depth = match qs.get("depth") {
        None => 1u32,
        Some(s) => match s.parse() {
            Ok(d) => d,
            Err(_) => return err_response("depth must be an integer"),
        },
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
    Json(body): Json<serde_json::Value>,
) -> Response {
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
async fn algo_wcc(State(state): State<AppState>, Json(body): Json<serde_json::Value>) -> Response {
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
    Json(body): Json<serde_json::Value>,
) -> Response {
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
}
