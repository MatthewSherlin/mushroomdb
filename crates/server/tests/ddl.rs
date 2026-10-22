//! Cypher DDL (0.10 TY-9) over the server surfaces: HTTP `/query` and the MCP
//! `query` tool. A full token runs `CREATE`/`DROP INDEX|CONSTRAINT` as a
//! write; a role token, a mask, a namespace and `as_of` refuse it, as they
//! refuse any write. `SHOW …` is a read on every path.

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
        "graphdb-srv-ddl-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A store with role `writer` (reads and writes label `P`), behind a full
/// token `full` and a role token `rtok`.
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
    let tokens: HashMap<String, String> = [("rtok".to_string(), "writer".to_string())].into();
    let app = server::router_with_role_tokens(db.clone(), Some("full".into()), tokens);
    (app, db)
}

async fn post(app: &Router, uri: &str, token: &str, body: Json) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn json(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("json: {e}: {}", String::from_utf8_lossy(bytes)))
}

#[tokio::test]
async fn a_full_token_runs_ddl_through_query_and_show_lists_it() {
    let (app, db) = open("full");
    // The default (Arrow) format and JSON both answer a DDL statement.
    let (status, body) = post(
        &app,
        "/query",
        "full",
        json!({"cypher": "CREATE INDEX i FOR (n:P) ON (n.a)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, body) = post(
        &app,
        "/query?format=json",
        "full",
        json!({"cypher": "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.b IS UNIQUE"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(db.read().range_indexes().len(), 1);
    assert_eq!(db.read().constraints().len(), 1);

    let (status, body) = post(
        &app,
        "/query?format=json",
        "full",
        json!({"cypher": "SHOW INDEXES"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rs = json(&body);
    assert_eq!(
        rs["columns"],
        json!(["name", "type", "label", "property", "state"])
    );
    assert_eq!(
        rs["rows"],
        json!([
            ["eq_P_b", "equality", "P", "b", "online"],
            ["i", "range", "P", "a", "online"]
        ])
    );
    let (status, body) = post(
        &app,
        "/query?format=json",
        "full",
        json!({"cypher": "SHOW CONSTRAINTS"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["rows"], json!([["c", "unique", "P", "b"]]));

    let (status, _) = post(
        &app,
        "/query",
        "full",
        json!({"cypher": "DROP CONSTRAINT c"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/query",
        "full",
        json!({"cypher": "DROP CONSTRAINT c"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("'c'"));
}

#[tokio::test]
async fn a_role_token_cannot_change_the_schema_but_can_show_it() {
    let (app, db) = open("role");
    db.write()
        .query_write("CREATE INDEX i FOR (n:P) ON (n.a)", &Default::default())
        .unwrap();
    for q in [
        "CREATE INDEX j FOR (n:P) ON (n.b)",
        "DROP INDEX i",
        "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE",
        "DROP CONSTRAINT c",
    ] {
        let (status, body) = post(&app, "/query?format=json", "rtok", json!({ "cypher": q })).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{q}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(db.read().range_indexes().len(), 1);
    assert!(db.read().constraints().is_empty());
    let (status, body) = post(
        &app,
        "/query?format=json",
        "rtok",
        json!({"cypher": "SHOW INDEXES"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json(&body)["rows"],
        json!([["i", "range", "P", "a", "online"]])
    );
}

#[tokio::test]
async fn as_of_a_mask_and_a_namespace_refuse_ddl() {
    let (app, db) = open("guards");
    for body in [
        json!({"cypher": "CREATE INDEX i FOR (n:P) ON (n.a)", "as_of": 0}),
        json!({"cypher": "CREATE INDEX i FOR (n:P) ON (n.a)", "mask": ["x"]}),
        json!({"cypher": "DROP INDEX i", "namespace": "default"}),
    ] {
        for token in ["full", "rtok"] {
            let (status, out) = post(&app, "/query?format=json", token, body.clone()).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{token} {body}: {}",
                String::from_utf8_lossy(&out)
            );
        }
    }
    assert!(db.read().range_indexes().is_empty());
}

// ── MCP `query` tool ────────────────────────────────────────────────────────

fn mcp_call(db: &SharedDb, arguments: Json) -> Json {
    let line = format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "query", "arguments": arguments}})
    );
    let mut reader = Cursor::new(line.into_bytes());
    let mut writer = Cursor::new(Vec::new());
    server::run_mcp_stdio(db.clone(), None, &mut reader, &mut writer).unwrap();
    let out = String::from_utf8(writer.into_inner()).unwrap();
    serde_json::from_str(out.lines().next().expect("one reply")).unwrap()
}

#[test]
fn the_mcp_query_tool_runs_ddl_and_a_role_cannot() {
    let (_, db) = open("mcp");
    let reply = mcp_call(&db, json!({"cypher": "CREATE INDEX i FOR (n:P) ON (n.a)"}));
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    assert_eq!(db.read().range_indexes().len(), 1);

    let reply = mcp_call(&db, json!({"cypher": "DROP INDEX i", "role": "writer"}));
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    assert_eq!(db.read().range_indexes().len(), 1);

    let reply = mcp_call(&db, json!({"cypher": "SHOW INDEXES", "role": "writer"}));
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    let rs: Json = serde_json::from_str(text).unwrap();
    assert_eq!(rs["rows"], json!([["i", "range", "P", "a", "online"]]));
}
