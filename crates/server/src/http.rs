//! Thin HTTP wrapper over [`SharedDb`]. Every endpoint is a lock, a public
//! core-api call, then a response — no business logic.

use crate::json::{params_from_json, result_set_json};
use crate::AppState;
use arrow_bridge::to_ipc_bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_api::{AutoFk, Dir, GraphError, IngestOptions, SharedDb};
use serde_json::{json, Value as Js};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Build the HTTP router over `db`. Read endpoints take the read lock;
/// `/ingest` takes the write lock. Guards are dropped before any `.await`.
/// `GET /watch` upgrades to a WebSocket fed by the post-commit sink.
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
        .route("/explain", get(explain))
        .route("/node/{key}/neighborhood", get(neighborhood))
        .route("/watch", get(crate::ws::watch))
        .with_state(state)
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if ready.send(local).is_err() {
        // Caller dropped the readiness receiver; still serve.
        eprintln!("serve: readiness receiver dropped before bind notify");
    }
    axum::serve(listener, router(db)).await
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

    let rs = {
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
    let rows_json = match serde_json::to_string(rows) {
        Ok(s) => s,
        Err(e) => return err_response(e.to_string()),
    };
    let opts = match ingest_options(body.get("options")) {
        Ok(o) => o,
        Err(e) => return err_response(e),
    };
    let report = {
        let mut g = state.db.write();
        g.ingest_json(&label, &rows_json, &opts)
    };
    match report {
        Ok(r) => match serde_json::to_value(&r) {
            Ok(v) => json_ok(v),
            Err(e) => err_response(e.to_string()),
        },
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
    let rs = {
        let g = state.db.read();
        match g.node_ref(&key) {
            Some(n) => Ok(n.neighborhood(depth, None, dir)),
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
