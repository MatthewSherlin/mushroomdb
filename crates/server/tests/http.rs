use arrow_array::{Array, StringArray};
use arrow_ipc::reader::StreamReader;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use core_api::{
    json_to_value, Explanation, FkSkip, IngestReport, Predicate, RuleDef, RuleStats, SharedDb,
    Stats, Value,
};
use serde_json::{json, Value as Json};
use server::{router, serve};
use std::io::Cursor;
use std::path::PathBuf;
use tower::ServiceExt;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-http-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(name: &str) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    (router(db.clone()), db)
}

async fn send(app: Router, req: Request<Body>) -> (StatusCode, Vec<u8>, Option<String>) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, ctype)
}

fn json_req(method: &str, uri: &str, body: Json) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn seed_person(db: &SharedDb, key: &str) {
    db.write()
        .insert_node("Person", key, vec![("id".into(), Value::Str(key.into()))])
        .unwrap();
}

fn parse_json(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("json: {e}: {}", String::from_utf8_lossy(bytes)))
}

/// Binding: POST /query default is Arrow IPC stream; StreamReader reads the batch.
#[tokio::test]
async fn query_default_returns_arrow_ipc() {
    let (app, db) = open("query-arrow");
    seed_person(&db, "p1");

    let (status, body, ctype) = send(
        app,
        json_req(
            "POST",
            "/query",
            json!({"cypher": "MATCH (t:Person {id: $tid}) RETURN t", "params": {"tid": "p1"}}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ctype.as_deref(),
        Some("application/vnd.apache.arrow.stream")
    );

    let mut reader = StreamReader::try_new(Cursor::new(body), None).unwrap();
    let batch = reader.next().expect("one batch").unwrap();
    assert!(reader.next().is_none(), "single batch stream");
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(batch.schema().field(0).name(), "t");
    let col = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(col.value(0), "p1");
}

/// Binding: ?format=json is columns + rows of JSON scalars, field-by-field.
#[tokio::test]
async fn query_format_json_matches_columns_and_rows() {
    let (app, db) = open("query-json");
    seed_person(&db, "p1");

    let (status, body, ctype) = send(
        app,
        json_req(
            "POST",
            "/query?format=json",
            json!({
                "cypher": "MATCH (t:Person {id: $tid}) RETURN t",
                "params": {"tid": "p1"}
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(v["columns"], json!(["t"]));
    assert_eq!(v["rows"], json!([["p1"]]));
}

/// Binding: bad Cypher is 400 {"error": detail} with a parse:-prefixed detail.
#[tokio::test]
async fn query_bad_cypher_is_400_with_parse_prefix() {
    let (app, _) = open("query-bad");

    let (status, body, _) = send(
        app,
        json_req("POST", "/query", json!({"cypher": "MATCH (n)"})),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.starts_with("parse:"),
        "expected parse:-prefixed detail, got {err}"
    );
}

/// Binding: GET /stats is Stats JSON; Serialize round-trips field-by-field.
#[tokio::test]
async fn stats_round_trips_serialize() {
    let (app, db) = open("stats");
    seed_person(&db, "p1");

    let (status, body, ctype) = send(app, get("/stats")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(v["nodes_live"], json!(1));
    assert_eq!(v["nodes_tombstoned"], json!(0));
    assert_eq!(v["edges"], json!(0));
    assert_eq!(v["rules"], json!([]));

    let live = Stats {
        nodes_live: 1,
        nodes_tombstoned: 2,
        edges: 3,
        rules: vec![RuleStats {
            name: "r".into(),
            edges: 4,
            tripped: true,
            fires: 5,
        }],
    };
    let encoded = serde_json::to_value(&live).expect("Stats: Serialize");
    assert_eq!(encoded["nodes_live"], json!(1));
    assert_eq!(encoded["nodes_tombstoned"], json!(2));
    assert_eq!(encoded["edges"], json!(3));
    assert_eq!(encoded["rules"][0]["name"], json!("r"));
    assert_eq!(encoded["rules"][0]["edges"], json!(4));
    assert_eq!(encoded["rules"][0]["tripped"], json!(true));
    assert_eq!(encoded["rules"][0]["fires"], json!(5));
}

/// Binding: POST /ingest converts parsed rows and returns IngestReport JSON.
#[tokio::test]
async fn ingest_happy_path() {
    let (app, db) = open("ingest-ok");
    db.write().insert_node("Org", "acme", vec![]).unwrap();

    let (status, body, ctype) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [{"id": "p1", "org_id": "acme", "name": "ada"}],
                "options": {}
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(v["inserted"], json!(1));
    assert_eq!(v["row_errors"], json!([]));
    assert_eq!(v["rules_created"], json!(["auto_fk_person_org_id"]));
    assert_eq!(v["skipped_fk_fields"], json!([]));
    assert!(db.read().has_node("p1"));
}

/// Binding: ingest shape failures (not an array of objects) are 400 {"error": ...}.
#[tokio::test]
async fn ingest_shape_error_is_400() {
    let (app, _) = open("ingest-shape");

    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({"label": "Person", "rows": {"id": "p1"}}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.contains("array of objects"),
        "expected ingest shape detail, got {err}"
    );
}

/// Binding: GET /explain?a=&b= is a JSON array of Explanation.
#[tokio::test]
async fn explain_happy_path() {
    let (app, db) = open("explain-ok");
    {
        let mut w = db.write();
        w.insert_node("Org", "o1", vec![]).unwrap();
        w.create_rule(RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("o1".into()))],
        )
        .unwrap();
    }

    let (status, body, ctype) = send(app, get("/explain?a=p1&b=o1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert!(v.is_array());
    assert_eq!(v[0]["rule"], json!("works_at"));
    assert_eq!(v[0]["edge_type"], json!("WORKS_AT"));
    assert_eq!(v[0]["src_key"], json!("p1"));
    assert_eq!(v[0]["dst_key"], json!("o1"));
    assert_eq!(v[0]["weight"], Json::Null);
}

/// Binding: unknown explain key is 400 {"error": ...}.
#[tokio::test]
async fn explain_unknown_key_is_400() {
    let (app, db) = open("explain-miss");
    seed_person(&db, "p1");

    let (status, body, _) = send(app, get("/explain?a=p1&b=ghost")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.contains("ghost"),
        "expected unknown key in detail, got {err}"
    );
}

/// Binding: neighborhood honors depth and dir; JSON is the ResultSet shape.
#[tokio::test]
async fn neighborhood_depth_and_dir() {
    let (app, db) = open("nbhd");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_node("Person", "c", vec![]).unwrap();
        w.insert_edge("KNOWS", "a", "b").unwrap();
        w.insert_edge("KNOWS", "b", "c").unwrap();
    }

    let (status, body, ctype) =
        send(app.clone(), get("/node/a/neighborhood?depth=1&dir=out")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let hop1 = parse_json(&body);
    assert_eq!(hop1["columns"], json!(["key", "label", "depth"]));
    assert_eq!(hop1["rows"], json!([["b", "Person", 1]]));

    let (_, body, _) = send(app.clone(), get("/node/a/neighborhood?depth=2&dir=out")).await;
    let hop2 = parse_json(&body);
    assert_eq!(
        hop2["rows"],
        json!([["b", "Person", 1], ["c", "Person", 2]])
    );

    let (_, body, _) = send(app, get("/node/b/neighborhood?depth=1&dir=in")).await;
    let incoming = parse_json(&body);
    assert_eq!(incoming["rows"], json!([["a", "Person", 1]]));
}

/// Binding: optional `edge_types` (comma-separated) filters like the MCP tool.
#[tokio::test]
async fn neighborhood_edge_types_filter() {
    let (app, db) = open("nbhd-etypes");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_node("Person", "c", vec![]).unwrap();
        w.insert_edge("KNOWS", "a", "b").unwrap();
        w.insert_edge("LIKES", "a", "c").unwrap();
    }

    let (status, body, _) = send(app.clone(), get("/node/a/neighborhood?depth=1&dir=out")).await;
    assert_eq!(status, StatusCode::OK);
    let unfiltered = parse_json(&body);
    assert_eq!(
        unfiltered["rows"],
        json!([["b", "Person", 1], ["c", "Person", 1]])
    );

    let (_, body, _) = send(
        app.clone(),
        get("/node/a/neighborhood?depth=1&dir=out&edge_types=KNOWS"),
    )
    .await;
    let knows = parse_json(&body);
    assert_eq!(knows["rows"], json!([["b", "Person", 1]]));

    let (_, body, _) = send(
        app,
        get("/node/a/neighborhood?depth=1&dir=out&edge_types=KNOWS,LIKES"),
    )
    .await;
    let both = parse_json(&body);
    assert_eq!(
        both["rows"],
        json!([["b", "Person", 1], ["c", "Person", 1]])
    );
}

/// Binding: unknown neighborhood key is 400.
#[tokio::test]
async fn neighborhood_unknown_key_is_400() {
    let (app, _) = open("nbhd-miss");
    let (status, body, _) = send(app, get("/node/ghost/neighborhood")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.contains("ghost"),
        "expected unknown key in detail, got {err}"
    );
}

/// Binding: serve binds port 0 and reports the local addr via oneshot readiness.
#[tokio::test]
async fn serve_readiness_returns_local_addr() {
    let db = SharedDb::open(&tmp("serve")).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        serve(db, "127.0.0.1:0".parse().unwrap(), tx).await.unwrap();
    });
    let addr = rx.await.expect("readiness");
    assert_ne!(addr.port(), 0, "ephemeral port must be resolved");
    handle.abort();
}

/// Binding: Serialize exists on the wire types (additive derives).
#[test]
fn wire_types_serialize() {
    let stats = Stats {
        nodes_live: 0,
        nodes_tombstoned: 0,
        edges: 0,
        rules: vec![],
    };
    serde_json::to_value(&stats).unwrap();
    serde_json::to_value(&RuleStats {
        name: "n".into(),
        edges: 0,
        tripped: false,
        fires: 0,
    })
    .unwrap();
    serde_json::to_value(&IngestReport {
        inserted: 0,
        row_errors: vec![],
        rules_created: vec![],
        skipped_fk_fields: vec![FkSkip {
            field: "org_id".into(),
            reason: "no matching target keys".into(),
        }],
    })
    .unwrap();
    serde_json::to_value(&Explanation {
        rule: "r".into(),
        edge_type: "E".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
        weight: Some(0.5),
    })
    .unwrap();
    assert_eq!(
        json_to_value(json!("p1")),
        Some(Value::Str("p1".into())),
        "params reuse json_to_value"
    );
}
