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
};
use crate::AppState;
use arrow_bridge::to_ipc_bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_api::{is_write_query, json_to_rows, AutoFk, Dir, GraphError, IngestOptions, RuleDef, SuggestConfig, SharedDb, SUGGEST_DEFAULT_SEED};
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
/// [`SharedDb`] uses a std [`std::sync::RwLock`]. Under a tokio multi-thread
/// runtime, `db.read()` / `db.write()` park the caller's worker thread for
/// the duration of contention. That is acceptable at embedded v1 scale.
/// Wrap these calls in `tokio::task::spawn_blocking` before exposing the
/// server to concurrent multi-client load.
pub fn router(db: SharedDb) -> Router {
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
    let state = AppState { db, watch: tx };
    Router::new()
        .route("/query", post(query))
        .route("/stats", get(stats))
        .route("/ingest", post(ingest))
        .route("/rules", post(create_rule))
        .route("/suggest", get(suggest))
        .route("/explain", get(explain))
        .route("/node/{key}", get(node_info))
        .route("/node/{key}/edges", get(node_edges))
        .route("/node/{key}/neighborhood", get(neighborhood))
        .route("/watch", get(crate::ws::watch))
        .route("/subscribe", get(crate::subscribe::subscribe))
        .with_state(state)
}

/// Same as [`router`], then `ServeDir` as the fallback so API routes win.
pub fn router_with_ui(db: SharedDb, ui_dir: impl AsRef<std::path::Path>) -> Router {
    router(db).fallback_service(ServeDir::new(ui_dir))
}

#[cfg(feature = "embed-ui")]
static EMBEDDED_UI: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

/// [`router`] plus the `embed-ui` static tree as fallback.
#[cfg(feature = "embed-ui")]
pub fn router_with_embedded_ui(db: SharedDb) -> Router {
    router(db).fallback(embedded_fallback)
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
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::None).await
}

/// [`serve`] plus a UI dist directory mounted behind the API routes.
pub async fn serve_with_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
    ui_dir: PathBuf,
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::Dir(ui_dir)).await
}

/// [`serve`] plus the compiled-in UI (no-op fallback if `embed-ui` is off).
#[cfg(feature = "embed-ui")]
pub async fn serve_with_embedded_ui(
    db: SharedDb,
    addr: SocketAddr,
    ready: tokio::sync::oneshot::Sender<SocketAddr>,
) -> std::io::Result<()> {
    serve_inner(db, addr, ready, UiFallback::Embedded).await
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
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        // Caller dropped the readiness receiver; still serve.
        eprintln!("serve: readiness receiver dropped before bind notify");
    }
    let app = match ui {
        UiFallback::None => router(db),
        UiFallback::Dir(dir) => router_with_ui(db, dir),
        #[cfg(feature = "embed-ui")]
        UiFallback::Embedded => router_with_embedded_ui(db),
    };
    axum::serve(listener, app).await
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

    // Detect write statements at the token level to dispatch to the correct lock.
    // Write statements (CREATE / MATCH…SET / MATCH…DELETE / MERGE) need the
    // write lock so mutations flow through WAL + rule engine with fsync before
    // the response is sent.  Read queries (MATCH … RETURN …) use the read lock.
    let is_write = match is_write_query(&cypher) {
        Ok(b) => b,
        Err(e) => return err_response(e),
    };

    let rs = if is_write {
        let mut g = state.db.write();
        g.query_write(&cypher, &params)
    } else {
        let g = state.db.read();
        g.query(&cypher, &params)
    };
    let rs = match rs {
        Ok(rs) => rs,
        Err(e) => return graph_err(e),
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
    let report = {
        let mut g = state.db.write();
        g.ingest_with_edges(&label, taken, &opts, &edges)
    };
    let report = report.map(|r| converted.into_report(r));
    match report {
        Ok(r) => match serde_json::to_value(&r) {
            Ok(v) => json_ok(v),
            Err(e) => err_response(e.to_string()),
        },
        Err(e) => graph_err(e),
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
        db.read().suggest_rules_with_config(&config, SUGGEST_DEFAULT_SEED)
    })
    .await
    {
        Ok(report) => json_ok(serde_json::to_value(&report).unwrap_or_else(|_| json!({}))),
        Err(_) => err_response("suggest task panicked"),
    }
}

async fn create_rule(State(state): State<AppState>, Json(body): Json<Js>) -> Response {
    let def: RuleDef = match serde_json::from_value(body) {
        Ok(d) => d,
        Err(e) => return err_response(e.to_string()),
    };
    let name = def.name.clone();
    let res = {
        let mut g = state.db.write();
        g.create_rule(def)
    };
    match res {
        Ok(()) => json_ok(json!({"ok": true, "name": name})),
        Err(e) => graph_err(e),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::result_set_json;
    use core_api::{ResultSet, Value};

    #[test]
    fn nan_float_cell_serializes_as_null() {
        let mut rs = ResultSet::new(vec!["n".into()]);
        rs.push_row(vec![Some(Value::Float(f64::NAN))]);
        let j = result_set_json(&rs);
        assert_eq!(j["rows"][0][0], Js::Null);
    }
}
