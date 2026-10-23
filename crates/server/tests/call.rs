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

/// `open`'s store plus a `Q` node, a `P`-`P` edge `LIKES`, a `HIDDEN` edge on
/// `Q`, and an index and a constraint on each label: the role `writer` reads
/// `P` only. With `cross`, `Q` is in namespace `a` with `P` and `HIDDEN` joins
/// `p1` to `q1`; without it, `q1` and `q2` are in namespace `b` and `HIDDEN`
/// joins those (an edge never crosses namespaces).
fn open_with_hidden(name: &str, cross: bool) -> (Router, SharedDb) {
    let (app, db) = open(name);
    {
        let mut w = db.write();
        let ns = |ns: &str| ("ns".to_string(), mushroomdb::Value::Str(ns.into()));
        let secret = |n: i64| ("secret".to_string(), mushroomdb::Value::Int(n));
        w.insert_node(
            "P",
            "p2",
            vec![ns("a"), ("shown".into(), mushroomdb::Value::Int(1))],
        )
        .unwrap();
        w.insert_edge("LIKES", "p1", "p2").unwrap();
        if cross {
            w.insert_node("Q", "q1", vec![ns("a"), secret(1)]).unwrap();
            w.insert_edge("HIDDEN", "p1", "q1").unwrap();
        } else {
            w.insert_node("Q", "q1", vec![ns("b"), secret(1)]).unwrap();
            w.insert_node("Q", "q2", vec![ns("b"), secret(2)]).unwrap();
            w.insert_edge("HIDDEN", "q1", "q2").unwrap();
        }
        for q in [
            "CREATE INDEX p_shown FOR (n:P) ON (n.shown)",
            "CREATE INDEX q_secret FOR (n:Q) ON (n.secret)",
            "CREATE CONSTRAINT p_unique FOR (n:P) REQUIRE n.shown IS UNIQUE",
            "CREATE CONSTRAINT q_unique FOR (n:Q) REQUIRE n.secret IS UNIQUE",
        ] {
            w.query_write(q, &Default::default()).unwrap();
        }
    }
    (app, db)
}

/// The first column of every row, as strings.
fn first_column(rows: &Json) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect()
}

const CATALOG: [&str; 6] = [
    "CALL db.labels()",
    "CALL db.relationshipTypes()",
    "CALL db.propertyKeys()",
    "CALL db.indexes()",
    "CALL db.constraints()",
    "CALL db.schema.visualization()",
];

/// Every catalog procedure answers through `ask` with `P`, `LIKES` and the
/// `P` index and constraint only.
fn assert_scoped(ask: &dyn Fn(&str) -> Json, at: &str) {
    let rows = |q: &str| ask(q)["rows"].clone();
    assert_eq!(first_column(&rows("CALL db.labels()")), ["P"], "{at}");
    assert_eq!(
        first_column(&rows("CALL db.relationshipTypes()")),
        ["LIKES"],
        "{at}"
    );
    let keys = first_column(&rows("CALL db.propertyKeys()"));
    assert!(keys.contains(&"shown".to_string()), "{at}: {keys:?}");
    assert!(!keys.contains(&"secret".to_string()), "{at}: {keys:?}");
    // The constraint's backing index is listed with the declared one.
    let mut indexes = first_column(&rows("CALL db.indexes()"));
    indexes.sort();
    assert_eq!(indexes, ["eq_P_shown", "p_shown"], "{at}");
    assert_eq!(
        first_column(&rows("CALL db.constraints()")),
        ["p_unique"],
        "{at}"
    );
    let picture = rows("CALL db.schema.visualization()").to_string();
    assert!(picture.contains("LIKES"), "{at}: {picture}");
    for hidden in ["Q", "HIDDEN", "secret"] {
        assert!(!picture.contains(hidden), "{at}: {hidden} in {picture}");
    }
}

/// An unscoped caller sees both labels' schema, so the fixture is not vacuous.
fn assert_unscoped(ask: &dyn Fn(&str) -> Json) {
    let rows = |q: &str| ask(q)["rows"].clone();
    assert_eq!(first_column(&rows("CALL db.labels()")), ["P", "Q"]);
    let mut constraints = first_column(&rows("CALL db.constraints()"));
    constraints.sort();
    assert_eq!(constraints, ["p_unique", "q_unique"]);
    assert!(first_column(&rows("CALL db.indexes()")).contains(&"q_secret".to_string()));
}

/// HTTP `/query` with `token` and the `extra` body fields, every procedure.
async fn http_answers(app: &Router, token: &str, extra: Json) -> HashMap<String, Json> {
    let mut answers = HashMap::new();
    for q in CATALOG {
        let mut body = json!({ "cypher": q });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, out) = post(app, token, body).await;
        assert_eq!(status, StatusCode::OK, "{token} {extra} {q}: {out}");
        answers.insert(q.to_string(), out);
    }
    answers
}

#[tokio::test]
async fn http_lists_only_the_schema_in_scope() {
    let (app, _db) = open_with_hidden("http-scope", true);
    let all = http_answers(&app, "full", json!({})).await;
    assert_unscoped(&|q| all[q].clone());
    for (token, extra, at) in [
        ("rtok", json!({}), "role token"),
        ("full", json!({"mask": ["p1", "p2"]}), "mask"),
        ("rtok", json!({"mask": ["p1", "p2"]}), "role token and mask"),
    ] {
        let answers = http_answers(&app, token, extra).await;
        assert_scoped(&|q| answers[q].clone(), &format!("http {at}"));
    }
    let (app, _db) = open_with_hidden("http-scope-ns", false);
    let all = http_answers(&app, "full", json!({})).await;
    assert_unscoped(&|q| all[q].clone());
    for (token, extra, at) in [
        ("full", json!({"namespace": "a"}), "namespace"),
        (
            "rtok",
            json!({"namespace": "a"}),
            "role token and namespace",
        ),
        ("rtok", json!({}), "role token"),
    ] {
        let answers = http_answers(&app, token, extra).await;
        assert_scoped(&|q| answers[q].clone(), &format!("http {at}"));
    }
}

fn mcp_ask(db: &SharedDb, extra: Json, q: &str) -> Json {
    let mut arguments = json!({ "cypher": q });
    for (k, v) in extra.as_object().unwrap() {
        arguments[k] = v.clone();
    }
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
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn mcp_lists_only_the_schema_in_scope() {
    let (_, db) = open_with_hidden("mcp-scope", true);
    assert_unscoped(&|q| mcp_ask(&db, json!({}), q));
    for (extra, at) in [
        (json!({"role": "writer"}), "role"),
        (json!({"mask": ["p1", "p2"]}), "mask"),
    ] {
        assert_scoped(&|q| mcp_ask(&db, extra.clone(), q), &format!("mcp {at}"));
    }
    let (_, db) = open_with_hidden("mcp-scope-ns", false);
    assert_unscoped(&|q| mcp_ask(&db, json!({}), q));
    for (extra, at) in [
        (json!({"namespace": "a"}), "namespace"),
        (
            json!({"role": "writer", "namespace": "a"}),
            "role and namespace",
        ),
    ] {
        assert_scoped(&|q| mcp_ask(&db, extra.clone(), q), &format!("mcp {at}"));
    }
}
