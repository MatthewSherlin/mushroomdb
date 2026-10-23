//! `CALL ns.name(args) [YIELD …]` (0.10 QL-12) over the server surfaces:
//! HTTP `/query` and the MCP `query` tool. A call is a read, so it answers for
//! a full token, a role token, `as_of`, a mask and a namespace alike, and the
//! refused forms come back as 400 with their name.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use mushroomdb::{schema::Schema, RoleDef, SharedDb, WriteScope};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use tower::ServiceExt;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-srv-call-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A store with one `P` node and role `writer`, behind a full token `full`
/// and a role token `rtok`.
fn open(name: &str) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                keys: vec![],
                labels: vec!["P".into()],
                visible_where: None,
                namespaces: None,
                write: Some(WriteScope {
                    create_labels: vec!["P".into()],
                    update_labels: vec!["P".into()],
                    delete_labels: vec!["P".into()],
                    create_edge_types: vec![],
                    delete_edge_types: vec![],
                    update_edge_types: vec![],
                }),
            }],
            ..Default::default()
        })
        .unwrap();
    db.write()
        .insert_node(
            "P",
            "p1",
            vec![("ns".into(), mushroomdb::Value::Str("a".into()))],
        )
        .unwrap();
    let tokens: HashMap<String, String> = [("rtok".to_string(), "writer".to_string())].into();
    let app = server::router_with_role_tokens(db.clone(), Some("full".into()), tokens);
    (app, db)
}

async fn post(app: &Router, token: &str, body: Json) -> (StatusCode, Json) {
    let req = Request::builder()
        .method("POST")
        .uri("/query?format=json")
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("json: {e}: {}", String::from_utf8_lossy(&bytes)));
    (status, body)
}

const PING: &str = "CALL db.ping() YIELD success AS ok RETURN ok";

#[tokio::test]
async fn http_query_runs_a_call_for_every_caller() {
    let (app, _db) = open("http");
    for (token, extra) in [
        ("full", json!({})),
        ("rtok", json!({})),
        ("full", json!({"as_of": 0})),
        ("full", json!({"mask": ["p1"]})),
        ("full", json!({"namespace": "a"})),
        ("rtok", json!({"namespace": "a"})),
    ] {
        let mut body = json!({ "cypher": PING });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, out) = post(&app, token, body).await;
        assert_eq!(status, StatusCode::OK, "{token} {extra}: {out}");
        assert_eq!(out["columns"], json!(["ok"]), "{token} {extra}");
        assert_eq!(out["rows"], json!([[true]]), "{token} {extra}");
    }
}

#[tokio::test]
async fn http_query_names_a_refused_form() {
    let (app, _db) = open("refused");
    for (q, want) in [
        ("CALL { MATCH (n) RETURN n }", "CALL subquery"),
        ("CALL unknown.proc()", "unknown procedure: unknown.proc"),
        (
            "MATCH (n) CALL db.ping() RETURN n",
            "CALL inside a larger query",
        ),
        ("FOREACH (x IN [1] | CREATE (n:X {id: x}))", "FOREACH"),
    ] {
        let (status, out) = post(&app, "full", json!({ "cypher": q })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{q}: {out}");
        assert!(out.to_string().contains(want), "{q}: {out}");
    }
}

#[test]
fn the_mcp_query_tool_runs_a_call() {
    let (_, db) = open("mcp");
    for arguments in [
        json!({ "cypher": PING }),
        json!({ "cypher": PING, "role": "writer" }),
    ] {
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                   "params": {"name": "query", "arguments": arguments}})
        );
        let mut reader = Cursor::new(line.into_bytes());
        let mut writer = Cursor::new(Vec::new());
        server::run_mcp_stdio(db.clone(), None, &mut reader, &mut writer).unwrap();
        let out = String::from_utf8(writer.into_inner()).unwrap();
        let reply: Json = serde_json::from_str(out.lines().next().expect("one reply")).unwrap();
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        let rs: Json = serde_json::from_str(text).unwrap();
        assert_eq!(rs["rows"], json!([[true]]), "{arguments}");
    }
}
