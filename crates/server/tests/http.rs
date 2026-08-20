use arrow_array::{Array, StringArray};
use arrow_ipc::reader::StreamReader;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use core_api::{
    json_to_value, Explanation, FkSkip, IngestReport, Predicate, PredicateSummary, RuleDef,
    RuleStats, SharedDb, Stats, Value,
};
use serde_json::{json, Value as Json};
#[cfg(feature = "embed-ui")]
use server::router_with_embedded_ui;
use server::{router, router_with_ui, serve};
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

/// Binding: COUNT(*) via ?format=json → {"columns":["COUNT(*)"],"rows":[[n]]}.
#[tokio::test]
async fn query_count_star_format_json_wire_shape() {
    let (app, db) = open("query-count-star");
    seed_person(&db, "p1");
    seed_person(&db, "p2");
    seed_person(&db, "p3");

    let (status, body, ctype) = send(
        app,
        json_req(
            "POST",
            "/query?format=json",
            json!({"cypher": "MATCH (p:Person) RETURN COUNT(*)"}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(v["columns"], json!(["COUNT(*)"]), "column name must be COUNT(*)");
    assert_eq!(v["rows"], json!([[3]]), "three Person nodes must be counted");
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
            approximate: false,
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
    assert_eq!(encoded["rules"][0]["approximate"], json!(false));
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
            approximate: false,
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
    assert_eq!(v[0]["predicate"]["kind"], json!("key_match"));
    assert_eq!(v[0]["predicate"]["fields"], json!(["org_id"]));
    let pred = v[0]["predicate"].as_object().expect("predicate object");
    for key in ["min", "tolerance", "km", "parts"] {
        assert!(
            pred.contains_key(key),
            "leaf JSON must present {key} as null, not omit it"
        );
        assert_eq!(pred[key], Json::Null);
    }
}

/// Binding: /explain JSON includes `predicate` with snake_case kind and parts.
#[tokio::test]
async fn explain_predicate_all_json_shape() {
    let (app, db) = open("explain-all");
    {
        let mut w = db.write();
        w.insert_node(
            "Org",
            "o1",
            vec![
                ("ind".into(), Value::Str("arch".into())),
                ("tags".into(), Value::List(vec![Value::Str("x".into())])),
            ],
        )
        .unwrap();
        w.create_rule(RuleDef {
            name: "both".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::All(vec![
                Predicate::FieldEqual {
                    field: "ind".into(),
                },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.5,
                },
            ]),
            edge_type: "BOTH".into(),
            weight_prop: Some("score".into()),
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("ind".into(), Value::Str("arch".into())),
                ("tags".into(), Value::List(vec![Value::Str("x".into())])),
            ],
        )
        .unwrap();
    }

    let (status, body, _) = send(app, get("/explain?a=p1&b=o1")).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v[0]["predicate"]["kind"], json!("all"));
    assert_eq!(v[0]["predicate"]["fields"], json!(["ind", "tags"]));
    assert_eq!(v[0]["predicate"]["parts"][0]["kind"], json!("field_equal"));
    assert_eq!(v[0]["predicate"]["parts"][1]["kind"], json!("overlap"));
    assert_eq!(v[0]["predicate"]["parts"][1]["min"], json!(0.5));
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

/// Binding: GET /node/{key} is NodeInfo JSON with untagged Value props.
#[tokio::test]
async fn node_info_json_shape() {
    let (app, db) = open("node-info");
    {
        let mut w = db.write();
        w.insert_node(
            "Person",
            "p1",
            vec![
                ("years".into(), Value::Int(8)),
                ("name".into(), Value::Str("ada".into())),
                ("ok".into(), Value::Bool(true)),
                ("rating".into(), Value::Float(0.5)),
                (
                    "tags".into(),
                    Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]),
                ),
            ],
        )
        .unwrap();
    }

    let (status, body, ctype) = send(app, get("/node/p1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(v["key"], json!("p1"));
    assert_eq!(v["label"], json!("Person"));
    assert_eq!(v["props"]["name"], json!("ada"));
    assert_eq!(v["props"]["ok"], json!(true));
    assert_eq!(v["props"]["years"], json!(8));
    assert_eq!(v["props"]["rating"], json!(0.5));
    assert_eq!(v["props"]["tags"], json!(["x", "y"]));
    // BTreeMap iteration is name-sorted; this assertion also depends on
    // serde_json's preserve_order feature (object keys keep insertion order).
    let keys: Vec<&str> = v["props"]
        .as_object()
        .expect("props object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["name", "ok", "rating", "tags", "years"]);
}

/// Binding: unknown GET /node/{key} is 404 {"error":"node key not found: ..."}.
#[tokio::test]
async fn node_info_unknown_key_is_404() {
    let (app, _) = open("node-info-miss");
    let (status, body, _) = send(app, get("/node/ghost")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v = parse_json(&body);
    assert_eq!(v, json!({"error": "node key not found: ghost"}));
}

/// Binding: GET /node/{key}/edges is {"edges":[EdgeInfo...]} with derived flags, sorted.
#[tokio::test]
async fn node_edges_json_shape_user_and_derived() {
    let (app, db) = open("node-edges");
    {
        let mut w = db.write();
        w.insert_node("Org", "acme", vec![]).unwrap();
        w.create_rule(core_api::RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: core_api::Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap();
        w.insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("acme".into()))],
        )
        .unwrap();
        w.insert_node("Person", "p2", vec![]).unwrap();
        w.insert_edge("KNOWS", "p1", "p2").unwrap();
    }

    let (status, body, ctype) = send(app, get("/node/p1/edges")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(
        v,
        json!({
            "edges": [
                {
                    "edge_type": "KNOWS",
                    "src_key": "p1",
                    "dst_key": "p2",
                    "derived": false
                },
                {
                    "edge_type": "WORKS_AT",
                    "src_key": "p1",
                    "dst_key": "acme",
                    "derived": true
                }
            ]
        })
    );
}

/// Binding: unknown GET /node/{key}/edges is 404 {"error":"node key not found: ..."}.
#[tokio::test]
async fn node_edges_unknown_key_is_404() {
    let (app, _) = open("node-edges-miss");
    let (status, body, _) = send(app, get("/node/ghost/edges")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v = parse_json(&body);
    assert_eq!(v, json!({"error": "node key not found: ghost"}));
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

/// Binding: ServeDir is the fallback; `/stats` stays JSON.
#[tokio::test]
async fn ui_fallback_serves_static_and_stats_stays_json() {
    let ui = tmp("ui-dist");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>graph-db</title>",
    )
    .unwrap();
    std::fs::write(ui.join("hello.txt"), "hello-static").unwrap();

    let db = SharedDb::open(&tmp("ui-api")).unwrap();
    let app = router_with_ui(db, &ui);

    let (st, body, _) = send(app.clone(), get("/hello.txt")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"hello-static");

    let (st, body, _) = send(app.clone(), get("/")).await;
    assert_eq!(st, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("graph-db"),
        "GET / should serve index.html, got {html}"
    );

    let (st, body, ctype) = send(app, get("/stats")).await;
    assert_eq!(st, StatusCode::OK);
    let ctype = ctype.expect("stats content-type");
    assert!(ctype.contains("json"), "/stats must stay JSON, got {ctype}");
    let j = parse_json(&body);
    assert!(
        j.get("nodes_live").is_some(),
        "/stats JSON must include nodes_live, got {j}"
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

/// Binding: POST /ingest optional `edges` shares the node ingest batch.
#[tokio::test]
async fn ingest_edges_inserts_user_edge() {
    let (app, db) = open("ingest-edges");
    db.write().insert_node("Person", "a", vec![]).unwrap();
    db.write().insert_node("Person", "b", vec![]).unwrap();

    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [],
                "edges": [{"edge_type": "KNOWS", "src": "a", "dst": "b"}]
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["edges_inserted"], json!(1));
    let edges = db.read().node_edges("a").unwrap();
    assert!(
        edges.iter().any(|e| {
            e.edge_type == "KNOWS" && e.src_key == "a" && e.dst_key == "b" && !e.derived
        }),
        "expected user KNOWS a→b, got {edges:?}"
    );
}

/// Binding: unknown src/dst on ingest edges is 400 with the engine key message.
#[tokio::test]
async fn ingest_edges_unknown_endpoint_is_400() {
    let (app, _) = open("ingest-edge-miss");
    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [],
                "edges": [{"edge_type": "KNOWS", "src": "ghost", "dst": "ghost"}]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().unwrap();
    assert!(
        err.contains("node key not found"),
        "expected KeyNotFound register, got {err}"
    );
}

/// Binding: a bad edge rejects the whole ingest; nodes from the same body
/// are not persisted.
#[tokio::test]
async fn ingest_bad_edge_is_atomic() {
    let (app, db) = open("ingest-atomic");
    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [{"id": "newbie"}],
                "edges": [{"edge_type": "KNOWS", "src": "newbie", "dst": "ghost"}]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().unwrap();
    assert!(
        err.contains("node key not found"),
        "preview error, got {err}"
    );
    assert!(
        !db.read().has_node("newbie"),
        "newbie must not persist after a rejected mixed batch"
    );
}

/// Binding: a duplicate user edge is a no-op and counts as 0 inserts.
#[tokio::test]
async fn ingest_duplicate_edge_counts_zero() {
    let (app, db) = open("ingest-dup-edge");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_edge("KNOWS", "a", "b").unwrap();
    }
    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [],
                "edges": [{"edge_type": "KNOWS", "src": "a", "dst": "b"}]
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["edges_inserted"], json!(0));
}

/// Binding: POST /rules accepts RuleDef JSON; validation errors are 400 verbatim.
#[tokio::test]
async fn create_rule_http_and_validation() {
    let (app, db) = open("rules-post");
    db.write()
        .insert_node("Org", "o1", vec![("founded_year".into(), Value::Int(2010))])
        .unwrap();
    db.write()
        .insert_node("Org", "o2", vec![("founded_year".into(), Value::Int(2011))])
        .unwrap();

    let (status, body, _) = send(
        app.clone(),
        json_req(
            "POST",
            "/rules",
            json!({
                "name": "founded_within",
                "src_label": "Org",
                "dst_label": "Org",
                "predicate": {"NumericWithin": {"field": "founded_year", "tolerance": 2.0}},
                "edge_type": "FOUNDED_WITHIN",
                "weight_prop": "score",
                "max_edges": null
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        parse_json(&body),
        json!({"ok": true, "name": "founded_within"})
    );
    assert!(db.read().rules().iter().any(|r| r.name == "founded_within"));

    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/rules",
            json!({
                "name": "",
                "src_label": "Org",
                "dst_label": "Org",
                "predicate": {"NumericWithin": {"field": "founded_year", "tolerance": 2.0}},
                "edge_type": "FOUNDED_WITHIN",
                "weight_prop": "score",
                "max_edges": null
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v = parse_json(&body);
    let err = v["error"].as_str().unwrap();
    assert!(
        err.contains("invalid rule:"),
        "engine message verbatim, got {err}"
    );
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
        approximate: false,
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
        edges_inserted: 0,
    })
    .unwrap();
    let expl = serde_json::to_value(&Explanation {
        rule: "r".into(),
        edge_type: "E".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
        weight: Some(0.5),
        predicate: PredicateSummary::from(&Predicate::KeyMatch { field: "fk".into() }),
    })
    .unwrap();
    assert_eq!(expl["predicate"]["kind"], json!("key_match"));
    assert_eq!(expl["predicate"]["fields"], json!(["fk"]));
    let pred = expl["predicate"].as_object().expect("predicate object");
    for key in ["min", "tolerance", "km", "parts"] {
        assert!(pred.contains_key(key), "wire summary must include {key}");
        assert_eq!(pred[key], Json::Null);
    }
    assert_eq!(
        json_to_value(json!("p1")),
        Some(Value::Str("p1".into())),
        "params reuse json_to_value"
    );
}

#[cfg(feature = "embed-ui")]
#[tokio::test]
async fn embedded_ui_serves_index_and_stats_wins() {
    let db = SharedDb::open(&tmp("embed-ui")).unwrap();
    let app = router_with_embedded_ui(db);
    let (st, body, ctype) = send(app.clone(), get("/")).await;
    assert_eq!(st, StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("mushroomdb") || html.contains("<!doctype") || html.contains("<!DOCTYPE"),
        "embedded GET / should be index.html, got {html}"
    );
    let ctype = ctype.unwrap_or_default();
    assert!(
        ctype.contains("html"),
        "index content-type html, got {ctype}"
    );

    let (st, body, ctype) = send(app, get("/stats")).await;
    assert_eq!(st, StatusCode::OK);
    let ctype = ctype.expect("stats content-type");
    assert!(ctype.contains("json"), "/stats must stay JSON, got {ctype}");
    let j = parse_json(&body);
    assert!(j.get("nodes_live").is_some(), "/stats JSON, got {j}");
}
