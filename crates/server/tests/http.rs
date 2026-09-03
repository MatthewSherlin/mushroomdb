#![allow(deprecated)] // serve() used for test convenience; production code uses serve_with_role_tokens
use arrow_array::{Array, StringArray};
use arrow_ipc::reader::StreamReader;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use core_api::{
    json_to_value, schema::Schema, Explanation, FkSkip, IngestReport, Predicate, PredicateSummary,
    RoleDef, RuleDef, RuleStats, SharedDb, Stats, Value, WriteScope,
};
use serde_json::{json, Value as Json};
#[cfg(feature = "embed-ui")]
use server::router_with_embedded_ui;
use server::{
    router, router_with_auth, router_with_role_tokens, router_with_ui, router_with_ui_tls, serve,
};
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

#[tokio::test]
async fn health_is_unauthenticated() {
    // boot with token Some("t"); GET /health must 200 without Authorization
    let db = SharedDb::open(&tmp("health-unauth")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, body, _) = send(app, get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["nodes"], json!(0));
    assert_eq!(v["edges"], json!(0));
    assert_eq!(v["addr"], json!("127.0.0.1:8080"));
}

#[tokio::test]
async fn health_reports_counts() {
    let (app, db) = open("health-counts");
    seed_person(&db, "a");
    seed_person(&db, "b");
    db.write().insert_edge("KNOWS", "a", "b").unwrap();
    let (status, body, _) = send(app, get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["nodes"], json!(2));
    assert_eq!(v["edges"], json!(1));
    assert_eq!(v["addr"], json!("127.0.0.1:8080"));
}

#[tokio::test]
async fn query_without_bearer_is_401_when_token_configured() {
    // POST /query with no header → 401 {"error":"..."}
    let db = SharedDb::open(&tmp("query-no-bearer")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, body, _) = send(
        app,
        json_req("POST", "/query", json!({"cypher": "MATCH (n) RETURN n"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v = parse_json(&body);
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "401 body must be {{\"error\":\"...\"}}, got {v}"
    );
}

#[tokio::test]
async fn query_with_bearer_succeeds_when_token_configured() {
    // Authorization: Bearer t → 200
    let db = SharedDb::open(&tmp("query-bearer")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let req = Request::builder()
        .method("POST")
        .uri("/query?format=json")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::AUTHORIZATION, "Bearer t")
        .body(Body::from(
            json!({"cypher": "MATCH (n) RETURN n"}).to_string(),
        ))
        .unwrap();
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn query_with_query_token_succeeds_when_token_configured() {
    let db = SharedDb::open(&tmp("query-qs-token")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, _, _) = send(
        app,
        json_req(
            "POST",
            "/query?token=t&format=json",
            json!({"cypher": "MATCH (n) RETURN n"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn query_with_wrong_query_token_is_401() {
    let db = SharedDb::open(&tmp("query-qs-wrong")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/query?token=wrong",
            json!({"cypher": "MATCH (n) RETURN n"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let v = parse_json(&body);
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "401 body must be {{\"error\":\"...\"}}, got {v}"
    );
}

#[tokio::test]
async fn watch_with_query_token_is_not_401_when_token_configured() {
    let db = SharedDb::open(&tmp("watch-qs-token")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, _, _) = send(app, get("/watch?token=t")).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "GET /watch?token=t must pass auth (upgrade may still fail without WS headers)"
    );
}

#[tokio::test]
async fn query_token_percent_decoded_matches_configured_token() {
    let db = SharedDb::open(&tmp("query-qs-encoded-slash")).unwrap();
    let app = router_with_auth(db, Some("a/b".into()));
    let (status, _, _) = send(
        app,
        json_req(
            "POST",
            "/query?token=a%2Fb&format=json",
            json!({"cypher": "MATCH (n) RETURN n"}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "configured token \"a/b\" must match URL-encoded ?token=a%2Fb"
    );

    let db = SharedDb::open(&tmp("query-qs-encoded-plus")).unwrap();
    let app = router_with_auth(db, Some("a+b".into()));
    let (status, _, _) = send(
        app,
        json_req(
            "POST",
            "/query?token=a%2Bb&format=json",
            json!({"cypher": "MATCH (n) RETURN n"}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "configured token \"a+b\" must match URL-encoded ?token=a%2Bb"
    );
}

#[tokio::test]
async fn watch_with_url_encoded_query_token_is_not_401() {
    let db = SharedDb::open(&tmp("watch-qs-encoded")).unwrap();
    let app = router_with_auth(db, Some("a/b".into()));
    let (status, _, _) = send(app, get("/watch?token=a%2Fb")).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "GET /watch?token=a%2Fb must pass auth for configured token \"a/b\""
    );
}

async fn send_headers(
    app: Router,
    req: Request<Body>,
) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body, headers)
}

#[tokio::test]
async fn html_query_token_sets_auth_cookie() {
    let ui = tmp("ui-cookie");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>graph-db</title>",
    )
    .unwrap();
    let db = SharedDb::open(&tmp("ui-cookie-db")).unwrap();
    let app = router_with_ui(db, &ui, Some("t".into()));
    let (status, _, headers) = send_headers(app, get("/?token=t")).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = headers
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cookie.contains("mushroomdb_token=t"),
        "Set-Cookie must include mushroomdb_token=, got {cookie:?}"
    );
    assert!(
        cookie.contains("Path=/"),
        "Set-Cookie Path=/, got {cookie:?}"
    );
    assert!(
        cookie.contains("SameSite=Lax"),
        "Set-Cookie SameSite=Lax, got {cookie:?}"
    );
    assert!(
        cookie.contains("HttpOnly"),
        "Set-Cookie HttpOnly, got {cookie:?}"
    );
    assert!(
        !cookie.contains("Secure"),
        "plain-HTTP router must not set Secure on cookie, got {cookie:?}"
    );
}

/// Cookie carries `; Secure` when the router is flagged as TLS-active,
/// even in an in-process test (no real TLS needed — the flag drives the attribute).
#[tokio::test]
async fn auth_cookie_is_secure_when_tls_active() {
    let ui = tmp("ui-cookie-tls");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>graph-db</title>",
    )
    .unwrap();
    let db = SharedDb::open(&tmp("ui-cookie-tls-db")).unwrap();
    let app = router_with_ui_tls(db, &ui, Some("t".into()));
    let (status, _, headers) = send_headers(app, get("/?token=t")).await;
    assert_eq!(status, StatusCode::OK);
    let cookie = headers
        .get(axum::http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cookie.contains("mushroomdb_token=t"),
        "Set-Cookie must include mushroomdb_token=, got {cookie:?}"
    );
    assert!(
        cookie.contains("; Secure"),
        "TLS-active router must include Secure attribute, got {cookie:?}"
    );
    assert!(
        cookie.contains("HttpOnly"),
        "Set-Cookie HttpOnly, got {cookie:?}"
    );
}

#[tokio::test]
async fn missing_asset_with_cookie_is_404_not_401() {
    let db = SharedDb::open(&tmp("asset-cookie")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let req = Request::builder()
        .method("GET")
        .uri("/no-such.js")
        .header(axum::http::header::COOKIE, "mushroomdb_token=t")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_asset_without_auth_is_401() {
    let db = SharedDb::open(&tmp("asset-noauth")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let (status, _, _) = send(app, get("/no-such.js")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn stats_with_cookie_succeeds_when_token_configured() {
    let db = SharedDb::open(&tmp("stats-cookie")).unwrap();
    let app = router_with_auth(db, Some("t".into()));
    let req = Request::builder()
        .method("GET")
        .uri("/stats")
        .header(axum::http::header::COOKIE, "mushroomdb_token=t")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert!(v.get("nodes_live").is_some(), "/stats JSON, got {v}");
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
    assert_eq!(
        v["columns"],
        json!(["COUNT(*)"]),
        "column name must be COUNT(*)"
    );
    assert_eq!(
        v["rows"],
        json!([[3]]),
        "three Person nodes must be counted"
    );
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

/// Binding: HTTP /ingest is one WAL commit, not a loop of insert_node.
#[tokio::test]
async fn ingest_many_rows_is_one_wal_commit() {
    let dir = tmp("ingest-one-wal");
    let db = SharedDb::open(&dir).unwrap();
    let app = router(db.clone());
    let (status, _, _) = send(
        app,
        json_req(
            "POST",
            "/ingest",
            json!({
                "label": "Person",
                "rows": [{"id": "a"}, {"id": "b"}, {"id": "c"}],
                "options": {"auto_fk": "off"}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(core_api::wal_commit_count_at(&dir).unwrap(), 1);
    assert_eq!(db.read().node_count(), 3);
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
            via_label: None,
            via_edge: None,
            via_dir: None,
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
    // works_at stores no weight prop; explain recomputes the KeyMatch score.
    assert_eq!(v[0]["weight"], json!(1.0));
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
            via_label: None,
            via_edge: None,
            via_dir: None,
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
            via_label: None,
            via_edge: None,
            via_dir: None,
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
    let app = router_with_ui(db, &ui, None);

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
        serve(db, "127.0.0.1:0".parse().unwrap(), tx, None)
            .await
            .unwrap();
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
    assert_eq!(
        db.read()
            .rules()
            .iter()
            .find(|r| r.name == "founded_within")
            .unwrap()
            .max_edges,
        Some(32),
        "JSON null max_edges fills default scored top-k"
    );

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

/// Binding: omitted JSON `max_edges` fills `default_max_edges` (scored=32, KeyMatch=1).
#[tokio::test]
async fn create_rule_http_omitted_max_edges_fills_default() {
    let (app, db) = open("rules-omit-max");
    db.write().insert_node("Org", "o1", vec![]).unwrap();
    db.write()
        .insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("o1".into()))],
        )
        .unwrap();

    let (status, _, _) = send(
        app.clone(),
        json_req(
            "POST",
            "/rules",
            json!({
                "name": "works_at",
                "src_label": "Person",
                "dst_label": "Org",
                "predicate": {"KeyMatch": {"field": "org_id"}},
                "edge_type": "WORKS_AT",
                "weight_prop": null
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        db.read()
            .rules()
            .iter()
            .find(|r| r.name == "works_at")
            .unwrap()
            .max_edges,
        Some(1)
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
        via_edge: None,
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

/// Cypher CREATE over HTTP uses the write lock, WAL is fsynced before the
/// response is sent, and the created node survives a DB re-open.
#[tokio::test]
async fn cypher_write_over_http_is_durable() {
    let dir = tmp("cypher-write-http");
    let db = SharedDb::open(&dir).unwrap();
    let app = router(db.clone());

    // CREATE via POST /query?format=json
    let (status, body, ctype) = send(
        app.clone(),
        json_req(
            "POST",
            "/query?format=json",
            json!({"cypher": "CREATE (n:Person {id: 'alice'})"}),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "write must succeed");
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let v = parse_json(&body);
    assert_eq!(
        v["columns"],
        json!(["created", "properties_set", "deleted"])
    );
    let rows = v["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], json!(1), "created=1");
    assert_eq!(rows[0][1], json!(0), "properties_set=0");
    assert_eq!(rows[0][2], json!(0), "deleted=0");

    // Read back via the same SharedDb handle to confirm node is live.
    assert!(
        db.read().has_node("alice"),
        "node must be queryable immediately"
    );

    // Drop the db + app, re-open from disk: WAL must have been fsynced.
    drop(app);
    drop(db);
    let db2 = SharedDb::open(&dir).unwrap();
    assert!(
        db2.read().has_node("alice"),
        "node must survive DB re-open (WAL fsynced before HTTP response)"
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

// ── HTTP params tests ─────────────────────────────────────────────────────────

/// Params round-trip over HTTP: $age filters nodes, returns correct rows.
#[tokio::test]
async fn http_params_read_round_trip() {
    let (app, db) = open("http-params-read");
    {
        let mut w = db.write();
        w.insert_node("HP", "alice", vec![("age".into(), Value::Int(30))])
            .unwrap();
        w.insert_node("HP", "bob", vec![("age".into(), Value::Int(25))])
            .unwrap();
    }

    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/query?format=json",
            json!({
                "cypher": "MATCH (n:HP) WHERE n.age = $age RETURN n",
                "params": {"age": 30}
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let rows = v["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1, "must match exactly the node with age=30");
    // The node key "alice" should appear in the result row.
    let row_str = rows[0].to_string();
    assert!(
        row_str.contains("alice"),
        "returned node must be alice: {row_str}"
    );
}

/// Injection safety at the HTTP layer: a param value containing Cypher syntax
/// must be treated as a literal string, not executed.
#[tokio::test]
async fn http_params_injection_safe() {
    let (app, db) = open("http-params-injection");
    {
        let mut w = db.write();
        w.insert_node("HPI", "real_node", vec![]).unwrap();
    }

    // The param value contains Cypher meta-characters.  If the value were
    // interpolated into the query string, the query would parse differently
    // and could return unexpected rows.  As a literal, no node has id equal
    // to the injection payload, so 0 rows are returned.
    let (status, body, _) = send(
        app,
        json_req(
            "POST",
            "/query?format=json",
            json!({
                "cypher": "MATCH (n:HPI {id: $id}) RETURN n",
                "params": {"id": "' RETURN 1//"}
            }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let rows = v["rows"].as_array().expect("rows array");
    assert_eq!(
        rows.len(),
        0,
        "injection payload must not return rows: {rows:?}"
    );
}

/// Write with SET n.p = $newval over HTTP is durable across DB re-open.
#[tokio::test]
async fn http_params_write_set_is_durable() {
    let dir = tmp("http-params-write");
    let db = SharedDb::open(&dir).unwrap();
    let app = router(db.clone());

    // Insert the node to update.
    {
        let mut w = db.write();
        w.insert_node("HPW", "target", vec![("score".into(), Value::Int(0))])
            .unwrap();
    }

    // MATCH…SET with $newval over HTTP.
    let (status, body, _) = send(
        app.clone(),
        json_req(
            "POST",
            "/query?format=json",
            json!({
                "cypher": "MATCH (n:HPW) WHERE n.score = 0 SET n.score = $newval",
                "params": {"newval": 99}
            }),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "write must succeed: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(v["rows"][0][1], json!(1), "properties_set must be 1");

    // Verify durability: drop handles and re-open.
    drop(app);
    drop(db);
    let db2 = SharedDb::open(&dir).unwrap();
    // Read back the updated score via a direct query.
    let rs = db2
        .read()
        .query(
            "MATCH (n:HPW) RETURN n.score",
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(
        rs.get(0, "n.score"),
        Some(&Value::Int(99)),
        "score must be 99 after re-open"
    );
}

// ── RBAC: role-bound server token tests ────────────────────────────────────────

/// Build a DB with named roles and return (Router, SharedDb) with the role
/// token map wired up.
///
/// `roles` is a slice of (role_name, allowed_labels, allowed_keys).
/// `full_token` is the full-access bearer value (None = no full token).
/// `role_token_map` is a slice of (bearer_value, role_name).
fn open_rbac(
    name: &str,
    roles: &[(&str, &[&str], &[&str])],
    full_token: Option<&str>,
    role_token_map: &[(&str, &str)],
) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    let schema = Schema {
        roles: roles
            .iter()
            .map(|(rname, labels, keys)| RoleDef {
                name: rname.to_string(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
                keys: keys.iter().map(|s| s.to_string()).collect(),
                write: None,
            })
            .collect(),
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();
    let rtoks: std::collections::HashMap<String, String> = role_token_map
        .iter()
        .map(|(tok, role)| (tok.to_string(), role.to_string()))
        .collect();
    let app = router_with_role_tokens(db.clone(), full_token.map(str::to_string), rtoks);
    (app, db)
}

/// Authenticated JSON POST.
fn authed_json_req(method: &str, uri: &str, token: &str, body: Json) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Authenticated GET.
fn authed_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Authenticated GET that includes WebSocket upgrade headers so the handler
/// can reach the identity check before failing on missing WS framing.
fn authed_ws_upgrade(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Version", "13")
        .body(Body::empty())
        .unwrap()
}

// ── 1. Role token sees only its subgraph via /query ───────────────────────────

#[tokio::test]
async fn role_token_query_sees_only_role_subgraph() {
    // analyst role: sees "Pub" label; "Secret" label is hidden.
    let (app, db) = open_rbac(
        "rbac-q-subgraph",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("analyst-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Pub", "pub2", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // Role token: only sees Pub nodes.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "analyst-tok",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "role must see only 2 Pub nodes, got {v}");

    // Full token: sees all three.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "admin",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(
        v["rows"].as_array().unwrap().len(),
        3,
        "full token must see all 3 nodes"
    );
}

// ── 2. Role token sees subgraph via /node/{key} ───────────────────────────────

#[tokio::test]
async fn role_token_node_visible_key_is_200() {
    let (app, db) = open_rbac(
        "rbac-node-ok",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();

    let (status, _, _) = send(app, authed_get("/node/pub1", "role-tok")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn role_token_node_hidden_key_is_404() {
    let (app, db) = open_rbac(
        "rbac-node-hidden",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    let (status, body, _) = send(app, authed_get("/node/sec1", "role-tok")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v = parse_json(&body);
    assert!(
        v["error"].as_str().is_some_and(|s| s.contains("sec1")),
        "404 body must name the key"
    );
}

// ── 3. Hidden key same response shape as truly absent key ────────────────────

#[tokio::test]
async fn role_token_hidden_key_indistinguishable_from_absent() {
    let (app, db) = open_rbac(
        "rbac-hidden-absent",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    let (hidden_status, hidden_body, _) =
        send(app.clone(), authed_get("/node/sec1", "role-tok")).await;
    let (absent_status, absent_body, _) =
        send(app, authed_get("/node/totally-absent", "role-tok")).await;

    assert_eq!(hidden_status, StatusCode::NOT_FOUND);
    assert_eq!(absent_status, StatusCode::NOT_FOUND);
    // Both bodies have the same shape: {"error":"node key not found: ..."}
    let hidden_v = parse_json(&hidden_body);
    let absent_v = parse_json(&absent_body);
    assert!(
        hidden_v["error"].as_str().is_some(),
        "hidden: must have error field"
    );
    assert!(
        absent_v["error"].as_str().is_some(),
        "absent: must have error field"
    );
}

// ── 4. Full token is unaffected by role config ────────────────────────────────

#[tokio::test]
async fn full_token_unaffected_by_role_config() {
    let (app, db) = open_rbac(
        "rbac-full-unaffected",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // Full token sees all nodes via /query.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "admin",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["rows"].as_array().unwrap().len(), 2);

    // Full token can write.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "admin",
        json!({"cypher": "CREATE (n:Pub {id: 'pub2'})"}),
    );
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK, "full token write must succeed");

    // Full token can read /stats.
    let (status, _, _) = send(app, authed_get("/stats", "admin")).await;
    assert_eq!(status, StatusCode::OK);
}

// ── 5. Write via role token → 403 ────────────────────────────────────────────

#[tokio::test]
async fn role_token_write_query_is_403() {
    let (app, _db) = open_rbac(
        "rbac-write-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "CREATE (n:Pub {id: 'evil'})"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "write via role token must be 403: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
}

// ── 6. /ingest via role token → 403 ──────────────────────────────────────────

#[tokio::test]
async fn role_token_ingest_is_403() {
    let (app, _db) = open_rbac(
        "rbac-ingest-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "Pub", "rows": [{"id": "x"}]}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── 7. /rules via role token → 403 ───────────────────────────────────────────

#[tokio::test]
async fn role_token_rules_is_403() {
    let (app, _db) = open_rbac(
        "rbac-rules-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/rules",
        "role-tok",
        json!({
            "name": "r",
            "src_label": "Pub",
            "dst_label": "Pub",
            "predicate": {"KeyMatch": {"field": "fk"}},
            "edge_type": "REL"
        }),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── 8. /subscribe via role token → 403 ───────────────────────────────────────

#[tokio::test]
async fn role_token_subscribe_is_403() {
    let (app, _db) = open_rbac(
        "rbac-sub-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    // Send a proper WS upgrade request so the handler body is reached.
    let req = authed_ws_upgrade("/subscribe", "role-tok");
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "/subscribe with role token must be 403: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
}

// ── 9. /watch via role token → 403 ───────────────────────────────────────────

#[tokio::test]
async fn role_token_watch_is_403() {
    let (app, _db) = open_rbac(
        "rbac-watch-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_ws_upgrade("/watch", "role-tok");
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "/watch with role token must be 403: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()));
}

// ── 10. /stats via role token → 403 ──────────────────────────────────────────

#[tokio::test]
async fn role_token_stats_is_403() {
    let (app, _db) = open_rbac(
        "rbac-stats-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let (status, _, _) = send(app, authed_get("/stats", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── 11. /algo/* via role token → 403 ─────────────────────────────────────────

#[tokio::test]
async fn role_token_algo_is_403() {
    let (app, _db) = open_rbac(
        "rbac-algo-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req("POST", "/algo/pagerank", "role-tok", json!({}));
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "/algo/pagerank must be 403");

    let req = authed_json_req("POST", "/algo/wcc", "role-tok", json!({}));
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "/algo/wcc must be 403");

    let req = authed_json_req("POST", "/algo/degree", "role-tok", json!({}));
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "/algo/degree must be 403");
}

// ── 12. /explain via role token → 403 ────────────────────────────────────────

#[tokio::test]
async fn role_token_explain_is_403() {
    let (app, _db) = open_rbac(
        "rbac-explain-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let (status, _, _) = send(app, authed_get("/explain?a=x&b=y", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── 13. /suggest via role token → 403 ────────────────────────────────────────

#[tokio::test]
async fn role_token_suggest_is_403() {
    let (app, _db) = open_rbac(
        "rbac-suggest-403",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let (status, _, _) = send(app, authed_get("/suggest", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── 14. Unknown bearer token (not in role_tokens and not full) → 401 ─────────

#[tokio::test]
async fn unknown_token_is_401() {
    let (app, _db) = open_rbac(
        "rbac-unknown-tok",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "not-a-real-token",
        json!({"cypher": "MATCH (n) RETURN n"}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ── 15. Token bound to unknown role → 401 ────────────────────────────────────

#[tokio::test]
async fn token_bound_to_unknown_role_is_401() {
    // "ghost-role" is not defined in the DB schema.
    let db = SharedDb::open(&tmp("rbac-unknown-role")).unwrap();
    // Apply schema with NO roles at all (or a different role).
    let schema = Schema {
        roles: vec![],
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();

    let rtoks = [("role-tok".to_string(), "ghost-role".to_string())]
        .into_iter()
        .collect();
    let app = router_with_role_tokens(db.clone(), Some("admin".to_string()), rtoks);

    db.write().insert_node("Pub", "pub1", vec![]).unwrap();

    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "token bound to unknown role must be 401, body: {}",
        String::from_utf8_lossy(&body)
    );
}

// ── 16. Client mask ∩ role mask: never widens ─────────────────────────────────

#[tokio::test]
async fn role_token_client_mask_intersects_role_mask() {
    // analyst sees "Pub" label; alice, bob, carol are Pub; dave is Secret.
    let (app, db) = open_rbac(
        "rbac-mask-intersect",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "alice", vec![]).unwrap();
    db.write().insert_node("Pub", "bob", vec![]).unwrap();
    db.write().insert_node("Pub", "carol", vec![]).unwrap();
    db.write().insert_node("Secret", "dave", vec![]).unwrap();

    // Client supplies mask [alice, bob, dave]. Role mask = [alice, bob, carol].
    // Effective = intersection = [alice, bob] only. dave must not appear.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({
            "cypher": "MATCH (n:Pub) RETURN n.id",
            "mask": ["alice", "bob", "dave"]
        }),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "only alice+bob must be visible, got {v}");

    // Client cannot widen by omitting mask: still role-masked.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        3,
        "role sees 3 Pub nodes without client mask, got {v}"
    );
}

// ── 17. Poisoned roles.json → 500 for role tokens ────────────────────────────

#[tokio::test]
async fn poisoned_sidecar_is_500_for_role_token() {
    let dir = tmp("rbac-poisoned");
    std::fs::create_dir_all(&dir).unwrap();
    // Write a syntactically invalid roles.json before opening the DB.
    std::fs::write(dir.join("roles.json"), b"not valid json at all!").unwrap();

    // SharedDb::open succeeds but roles are poisoned (mask_for_role returns Err).
    let db = SharedDb::open(&dir).unwrap();

    let rtoks = [("role-tok".to_string(), "analyst".to_string())]
        .into_iter()
        .collect();
    // Full token is still functional.
    let app = router_with_role_tokens(db.clone(), Some("admin".to_string()), rtoks);

    // Role token → 500 (poisoned state).
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "poisoned sidecar must be 500 for role token: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "500 body must have error field"
    );

    // Full token is unaffected.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "admin",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "full token must be unaffected by poisoned sidecar"
    );
}

// ── 18. No role config: existing behavior byte-identical ─────────────────────

#[tokio::test]
async fn no_role_config_is_byte_identical() {
    // A router with no role tokens and no full token behaves exactly as before.
    let (app, db) = open("no-rbac-compat");
    db.write().insert_node("P", "x", vec![]).unwrap();
    let req = json_req(
        "POST",
        "/query?format=json",
        json!({"cypher": "MATCH (n) RETURN n.id"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["rows"].as_array().unwrap().len(), 1);
}

// ── 19. /node/{key}/edges hidden key → 404 ───────────────────────────────────

#[tokio::test]
async fn role_token_node_edges_hidden_key_is_404() {
    let (app, db) = open_rbac(
        "rbac-edges-hidden",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    let (status, _, _) = send(app, authed_get("/node/sec1/edges", "role-tok")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── 20. /node/{key}/neighborhood hidden key → 404 ────────────────────────────

#[tokio::test]
async fn role_token_neighborhood_hidden_key_is_404() {
    let (app, db) = open_rbac(
        "rbac-nbhd-hidden",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    let (status, _, _) = send(app, authed_get("/node/sec1/neighborhood", "role-tok")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Fix round-1: C1 — /edges filters hidden neighbor endpoints ────────────────

#[tokio::test]
async fn role_token_edges_filters_hidden_neighbor() {
    // pub1 --KNOWS--> sec1 (hidden); pub1 --KNOWS--> pub2 (visible).
    // Role token on /node/pub1/edges must NOT see the edge to sec1.
    let (app, db) = open_rbac(
        "rbac-edges-filter",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    {
        let mut w = db.write();
        w.insert_node("Pub", "pub1", vec![]).unwrap();
        w.insert_node("Pub", "pub2", vec![]).unwrap();
        w.insert_node("Secret", "sec1", vec![]).unwrap();
        w.insert_edge("KNOWS", "pub1", "sec1").unwrap();
        w.insert_edge("KNOWS", "pub1", "pub2").unwrap();
    }

    let (status, body, _) = send(app.clone(), authed_get("/node/pub1/edges", "role-tok")).await;
    assert_eq!(status, StatusCode::OK, "visible entry key must be 200");
    let v = parse_json(&body);
    let edges = v["edges"].as_array().expect("edges array");
    // Only the pub1→pub2 edge should appear; the pub1→sec1 edge must be absent.
    assert_eq!(
        edges.len(),
        1,
        "role token must not see edge to hidden node"
    );
    let dst = edges[0]["dst_key"].as_str().unwrap_or("");
    assert_eq!(dst, "pub2", "only visible-neighbor edge returned");

    // Full token sees both edges.
    let (status, body, _) = send(app, authed_get("/node/pub1/edges", "admin")).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(
        v["edges"].as_array().unwrap().len(),
        2,
        "full token must see all edges"
    );
}

// ── Fix round-1: C2 — /neighborhood never leaks hidden nodes or routes through them

#[tokio::test]
async fn role_token_neighborhood_hides_hidden_nodes_and_does_not_route_through_them() {
    // Graph: pub1 --KNOWS--> sec1 (hidden) --KNOWS--> pub2 (visible, but only
    // reachable through a hidden intermediate).
    //
    // At depth 2 the unmasked BFS would yield both sec1 and pub2.
    // A role token must see neither (sec1 is hidden; pub2 is only reachable via sec1).
    let (app, db) = open_rbac(
        "rbac-nbhd-route",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    {
        let mut w = db.write();
        w.insert_node("Pub", "pub1", vec![]).unwrap();
        w.insert_node("Secret", "sec1", vec![]).unwrap();
        w.insert_node("Pub", "pub2", vec![]).unwrap();
        w.insert_edge("KNOWS", "pub1", "sec1").unwrap();
        w.insert_edge("KNOWS", "sec1", "pub2").unwrap();
    }

    let (status, body, _) = send(
        app.clone(),
        authed_get("/node/pub1/neighborhood?depth=2", "role-tok"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "visible entry key must be 200");
    let v = parse_json(&body);
    let rows = v["rows"].as_array().expect("rows array");
    // Neither sec1 nor pub2 should appear.
    let keys: Vec<&str> = rows
        .iter()
        .filter_map(|r| r.as_array()?.first()?.as_str())
        .collect();
    assert!(
        !keys.contains(&"sec1"),
        "hidden node sec1 must not appear in neighborhood"
    );
    assert!(
        !keys.contains(&"pub2"),
        "pub2 reachable only via hidden intermediate must not appear"
    );

    // Full token at depth 2 sees both sec1 and pub2.
    let (status, body, _) = send(app, authed_get("/node/pub1/neighborhood?depth=2", "admin")).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let full_rows = v["rows"].as_array().unwrap();
    let full_keys: Vec<&str> = full_rows
        .iter()
        .filter_map(|r| r.as_array()?.first()?.as_str())
        .collect();
    assert!(
        full_keys.contains(&"sec1"),
        "full token must see sec1 in neighborhood"
    );
    assert!(
        full_keys.contains(&"pub2"),
        "full token must see pub2 in neighborhood"
    );
}

// ── Fix round-1: I1 — serve_with_embedded_ui wires role tokens (unit check) ──

/// Validates that `router_with_role_tokens` (used by the embed-ui serve path)
/// enforces role tokens when invoked with a role map.  The embed-ui feature gate
/// guards `serve_with_embedded_ui` itself; this test covers the router logic that
/// `serve_with_embedded_ui` delegates to.
#[tokio::test]
async fn embed_ui_router_path_honors_role_tokens() {
    // open_rbac uses router_with_role_tokens — the same path serve_with_embedded_ui
    // now delegates to — so this exercises the fixed code path.
    let (app, db) = open_rbac(
        "rbac-embed-ui-wire",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // Role token: visible node is 200; hidden node is 404.
    let (st, _, _) = send(app.clone(), authed_get("/node/pub1", "role-tok")).await;
    assert_eq!(st, StatusCode::OK, "role token must see pub1");
    let (st, _, _) = send(app.clone(), authed_get("/node/sec1", "role-tok")).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "role token must not see sec1");

    // Unknown token is 401 even on this path.
    let (st, _, _) = send(app, authed_get("/node/pub1", "bad-tok")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "unknown token must be 401");
}

// ── Fix round-1: M1 — indistinguishability extends to /edges and /neighborhood ─

#[tokio::test]
async fn role_token_hidden_key_indistinguishable_from_absent_on_edges_and_neighborhood() {
    let (app, db) = open_rbac(
        "rbac-indist-ext",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // /edges: hidden and absent both return 404 with an error field.
    let (hidden_st, hidden_body, _) =
        send(app.clone(), authed_get("/node/sec1/edges", "role-tok")).await;
    let (absent_st, absent_body, _) = send(
        app.clone(),
        authed_get("/node/totally-absent/edges", "role-tok"),
    )
    .await;
    assert_eq!(hidden_st, StatusCode::NOT_FOUND);
    assert_eq!(absent_st, StatusCode::NOT_FOUND);
    let hv = parse_json(&hidden_body);
    let av = parse_json(&absent_body);
    // Both must have an "error" field with the same prefix pattern.
    let h_err = hv["error"].as_str().unwrap_or("");
    let a_err = av["error"].as_str().unwrap_or("");
    assert!(
        h_err.starts_with("node key not found"),
        "hidden /edges error must match absent prefix: {h_err}"
    );
    assert!(
        a_err.starts_with("node key not found"),
        "absent /edges error must match pattern: {a_err}"
    );

    // /neighborhood: same.
    let (hidden_st, hidden_body, _) = send(
        app.clone(),
        authed_get("/node/sec1/neighborhood", "role-tok"),
    )
    .await;
    let (absent_st, absent_body, _) = send(
        app,
        authed_get("/node/totally-absent/neighborhood", "role-tok"),
    )
    .await;
    assert_eq!(hidden_st, StatusCode::NOT_FOUND);
    assert_eq!(absent_st, StatusCode::NOT_FOUND);
    let hv = parse_json(&hidden_body);
    let av = parse_json(&absent_body);
    let h_err = hv["error"].as_str().unwrap_or("");
    let a_err = av["error"].as_str().unwrap_or("");
    assert!(
        h_err.starts_with("node key not found"),
        "hidden /neighborhood error must match absent prefix: {h_err}"
    );
    assert!(
        a_err.starts_with("node key not found"),
        "absent /neighborhood error must match pattern: {a_err}"
    );
}

// ── Fix round-1: M2 — poisoned sidecar on /node, /edges, /neighborhood → 500 ──

#[tokio::test]
async fn poisoned_sidecar_is_500_on_node_edges_and_neighborhood() {
    let dir = tmp("rbac-poisoned-ext");
    {
        let db = SharedDb::open(&dir).unwrap();
        db.write().insert_node("Pub", "pub1", vec![]).unwrap();
        db.write().insert_node("Pub", "pub2", vec![]).unwrap();
        db.write().insert_edge("KNOWS", "pub1", "pub2").unwrap();
    }
    // Poison the sidecar after nodes are inserted.
    std::fs::write(dir.join("roles.json"), b"not valid json").unwrap();
    let db = SharedDb::open(&dir).unwrap();
    let rtoks: std::collections::HashMap<String, String> =
        [("role-tok".to_string(), "analyst".to_string())]
            .into_iter()
            .collect();
    let app = router_with_role_tokens(db, Some("admin".to_string()), rtoks);

    for path in ["/node/pub1", "/node/pub1/edges", "/node/pub1/neighborhood"] {
        let (status, body, _) = send(app.clone(), authed_get(path, "role-tok")).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "poisoned sidecar must be 500 for role token on {path}: {}",
            String::from_utf8_lossy(&body)
        );
        let v = parse_json(&body);
        assert!(
            v["error"].as_str().is_some_and(|s| !s.is_empty()),
            "500 body must have error field on {path}"
        );
    }

    // Full token is unaffected.
    let (status, _, _) = send(app, authed_get("/node/pub1", "admin")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "full token must be unaffected by poisoned sidecar"
    );
}

// ── Existing non-RBAC test below (unchanged) ─────────────────────────────────

#[tokio::test]
async fn query_mask_filters_nodes() {
    let (app, db) = open("mask-http");
    db.write().insert_node("P", "alice", vec![]).unwrap();
    db.write().insert_node("P", "bob", vec![]).unwrap();
    db.write().insert_node("P", "carol", vec![]).unwrap();

    // With mask: only alice+bob visible.
    let req = json_req(
        "POST",
        "/query?format=json",
        json!({"cypher": "MATCH (n:P) RETURN n.id", "mask": ["alice", "bob"]}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["rows"].as_array().unwrap().len(), 2);

    // Write through mask → 400.
    let req = json_req(
        "POST",
        "/query?format=json",
        json!({"cypher": "CREATE (n:P {id: 'evil'})", "mask": ["alice"]}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "masked write must be 400: {}",
        String::from_utf8_lossy(&body)
    );

    // Without mask: all three visible.
    let req = json_req(
        "POST",
        "/query?format=json",
        json!({"cypher": "MATCH (n:P) RETURN n.id"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(v["rows"].as_array().unwrap().len(), 3);
}

/// POST /query with a client mask and a write Cypher must return exactly
/// 400 Bad Request with body `{"error":"masked queries are read-only"}`.
/// Pins the MaskedReadOnly → HTTP mapping so the arm cannot be silently removed.
#[tokio::test]
async fn masked_write_returns_400_with_exact_body() {
    let (app, db) = open("masked-write-pin");
    db.write().insert_node("P", "alice", vec![]).unwrap();

    let req = json_req(
        "POST",
        "/query?format=json",
        json!({"cypher": "CREATE (n:P {id: 'evil'})", "mask": ["alice"]}),
    );
    let (status, body, _) = send(app, req).await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "masked write must be 400; body: {}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("body must be valid JSON");
    assert_eq!(
        v,
        json!({"error": "masked queries are read-only"}),
        "response body must be exactly {{\"error\":\"masked queries are read-only\"}}"
    );
}

// ── History endpoints ─────────────────────────────────────────────────────────

/// Build a DB with two Person nodes and a manual LINK edge for history tests.
fn open_history_db(name: &str) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    db.write()
        .insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.write().insert_node("Person", "bob", vec![]).unwrap();
    db.write().insert_edge("LINK", "alice", "bob").unwrap();
    (router(db.clone()), db)
}

// ── Full-token happy paths ────────────────────────────────────────────────────

#[tokio::test]
async fn node_history_full_token_happy_path() {
    let (app, _db) = open_history_db("hist-nh-full");
    let (status, body, _) = send(app, get("/node/alice/history")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = parse_json(&body);

    assert_eq!(v["key"], "alice");
    let total = v["total_commits"].as_u64().expect("total_commits present");
    assert!(total > 0, "total_commits must be > 0");

    let history = v["history"].as_array().expect("history array");
    assert!(
        !history.is_empty(),
        "alice must have at least one history event"
    );

    // First event is NodeInserted.
    assert_eq!(history[0]["change"]["type"], "NodeInserted");
    assert_eq!(history[0]["change"]["label"], "Person");
}

#[tokio::test]
async fn edge_history_full_token_happy_path() {
    let (app, _db) = open_history_db("hist-eh-full");
    let (status, body, _) = send(app, get("/history/edge?a=alice&b=bob")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = parse_json(&body);

    assert_eq!(v["a"], "alice");
    assert_eq!(v["b"], "bob");
    let total = v["total_commits"].as_u64().expect("total_commits present");
    assert!(total > 0, "total_commits must be > 0");

    let events = v["events"].as_array().expect("events array");
    assert!(
        !events.is_empty(),
        "alice-bob must have at least one edge event"
    );

    // The LINK edge must appear as Added.
    let has_link = events
        .iter()
        .any(|ev| ev["edge_type"] == "LINK" && ev["event"] == "Added");
    assert!(has_link, "expected LINK Added event: {events:?}");
}

#[tokio::test]
async fn was_linked_full_token_happy_path() {
    let (app, db) = open_history_db("hist-wl-full");
    let total = db.read().wal_total_commits().unwrap();
    let (status, body, _) = send(
        app,
        get(&format!(
            "/history/was_linked?a=alice&b=bob&edge_type=LINK&at_commit={}",
            total - 1
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = parse_json(&body);
    assert_eq!(v["linked"], true);
    assert_eq!(v["a"], "alice");
    assert_eq!(v["edge_type"], "LINK");
}

#[tokio::test]
async fn was_linked_out_of_horizon_is_400_not_500() {
    let (app, _db) = open_history_db("hist-wl-oob");
    let (status, body, _) = send(
        app,
        get("/history/was_linked?a=alice&b=bob&edge_type=LINK&at_commit=99999"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "out-of-horizon must be 400: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|s| s.contains("range") || s.contains("out of")),
        "error must mention range: {v}"
    );
}

// ── Role-token cases ──────────────────────────────────────────────────────────

#[tokio::test]
async fn role_token_node_history_visible_key_is_200() {
    let (app, db) = open_rbac(
        "hist-nh-role-ok",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Person", "alice", vec![]).unwrap();

    let (status, body, _) = send(app, authed_get("/node/alice/history", "role-tok")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = parse_json(&body);
    assert_eq!(v["key"], "alice");
    assert!(
        v["total_commits"].as_u64().is_some(),
        "horizon field must be present"
    );
}

#[tokio::test]
async fn role_token_node_history_hidden_key_is_404() {
    let (app, db) = open_rbac(
        "hist-nh-role-hidden",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    let (status, body, _) = send(app, authed_get("/node/sec1/history", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "hidden key must be 404: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(
        v,
        json!({"error": "node key not found: sec1"}),
        "body must match key_not_found shape"
    );
}

#[tokio::test]
async fn role_token_edge_history_both_visible_is_200() {
    let (app, db) = open_rbac(
        "hist-eh-role-ok",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Person", "alice", vec![]).unwrap();
    db.write().insert_node("Person", "bob", vec![]).unwrap();
    db.write().insert_edge("KNOWS", "alice", "bob").unwrap();

    let (status, body, _) = send(app, authed_get("/history/edge?a=alice&b=bob", "role-tok")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = parse_json(&body);
    assert!(
        v["total_commits"].as_u64().is_some(),
        "horizon field must be present"
    );
}

#[tokio::test]
async fn role_token_edge_history_one_hidden_is_404() {
    let (app, db) = open_rbac(
        "hist-eh-role-hidden",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();
    db.write().insert_edge("LINK", "pub1", "sec1").unwrap();

    // sec1 is hidden for the role token; should get 404 (same-as-absent).
    let (status, body, _) = send(app, authed_get("/history/edge?a=pub1&b=sec1", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "hidden endpoint must give 404: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(
        v,
        json!({"error": "node key not found: sec1"}),
        "body must match key_not_found shape"
    );
}

#[tokio::test]
async fn role_token_history_write_denied() {
    // Confirm that a role token is rejected (403 FORBIDDEN) on write endpoints.
    // POST /ingest is a real write endpoint guarded by RBAC — this tests the
    // auth middleware, not Axum routing behaviour.
    let (app, _db) = open_rbac(
        "hist-role-write-denied",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "Person", "rows": [{"id": "x"}]}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "role token must be 403 on POST /ingest"
    );
}

// ── Fix round-2: C1 — node_history Role branch must filter hidden edge neighbors

/// C1 leak-repro: a VISIBLE node's history must NOT reveal hidden neighbor keys
/// in EdgeAdded/EdgeRemoved entries when accessed by a role token.
///
/// Setup: visible Person/alice + hidden Secret/shadow + LINK edge alice→shadow.
/// Role token (can see Person, not Secret): GET /node/alice/history must return
/// history with no mention of "shadow" in any entry.
/// Full token: the same request MUST include the EdgeAdded entry naming "shadow".
#[tokio::test]
async fn role_token_node_history_filters_hidden_edge_neighbor() {
    let (app, db) = open_rbac(
        "hist-nh-c1-leak",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    {
        let mut w = db.write();
        w.insert_node("Person", "alice", vec![]).unwrap();
        w.insert_node("Secret", "shadow", vec![]).unwrap();
        w.insert_edge("LINK", "alice", "shadow").unwrap();
    }

    // Role token: alice is visible, shadow is not — the EdgeAdded entry for the
    // LINK edge names "shadow" and must be stripped.
    let (status, body, _) = send(app.clone(), authed_get("/node/alice/history", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "visible node must be 200 for role token: {}",
        String::from_utf8_lossy(&body)
    );
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("shadow"),
        "role token response must NOT contain hidden key 'shadow': {body_str}"
    );

    // Full token: same node, same edge — the EdgeAdded entry naming "shadow"
    // must be present (unfiltered).
    let (status, body, _) = send(app, authed_get("/node/alice/history", "admin")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("shadow"),
        "full token response MUST contain edge neighbor key 'shadow': {body_str}"
    );
}

/// I2: GET /node/{key}/history for an absent key must return 404 (same as GET
/// /node/{key}) for BOTH Full and Role identities.
#[tokio::test]
async fn node_history_absent_key_is_404_both_identities() {
    let (app, _db) = open_rbac(
        "hist-nh-absent-404",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    // Full token: absent key → 404.
    let (status, body, _) = send(app.clone(), authed_get("/node/ghost/history", "admin")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "full token: absent key must be 404: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(v, json!({"error": "node key not found: ghost"}));

    // Role token: absent key → 404 (indistinguishable from hidden).
    let (status, body, _) = send(app, authed_get("/node/ghost/history", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "role token: absent key must be 404: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(v, json!({"error": "node key not found: ghost"}));
}

// ── Restricted-stub mask mode (Task 1: KB-hardening) ─────────────────────────

/// POST /query with a client mask and `stub_hidden: true` must be accepted
/// and return the same Cypher results as without the flag — hidden nodes are
/// excluded from query results in both modes (Cypher behaviour is mode-agnostic).
#[tokio::test]
async fn client_mask_stub_hidden_query_round_trip() {
    let (app, db) = open("stub-query-rt");
    {
        let mut g = db.write();
        g.insert_node("P", "alice", vec![]).unwrap();
        g.insert_node("P", "bob", vec![]).unwrap();
        g.insert_node("P", "carol", vec![]).unwrap();
        g.insert_edge("KNOWS", "alice", "bob").unwrap();
        g.insert_edge("KNOWS", "alice", "carol").unwrap();
    }

    // With mask=["alice","carol"] and stub_hidden=true, Cypher still sees only
    // the masked subgraph.  alice→carol is the only visible KNOWS edge.
    let req = json_req(
        "POST",
        "/query?format=json",
        json!({
            "cypher": "MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id",
            "mask": ["alice", "carol"],
            "stub_hidden": true
        }),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "stub_hidden request must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only alice→carol must be visible: {v}");

    // Same result without stub_hidden (Omit is default).
    let req2 = json_req(
        "POST",
        "/query?format=json",
        json!({
            "cypher": "MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id",
            "mask": ["alice", "carol"],
        }),
    );
    let (status2, body2, _) = send(app, req2).await;
    assert_eq!(status2, StatusCode::OK);
    let v2 = parse_json(&body2);
    assert_eq!(
        v2["rows"].as_array().unwrap().len(),
        1,
        "Omit mode must return same row count"
    );
}

/// GET /node/{key}?mask=alice,carol&stub_hidden=true on a hidden key must
/// return `{"key": "<key>", "restricted": true}` — not 404.
/// Full body must contain ONLY `key` and `restricted` (no label, no props).
#[tokio::test]
async fn client_mask_stub_node_info_hidden_key_returns_restricted_stub() {
    let (app, db) = open("stub-node-info-http");
    {
        let mut g = db.write();
        g.insert_node("P", "alice", vec![("score".into(), Value::Int(99))])
            .unwrap();
        g.insert_node("P", "bob", vec![]).unwrap();
    }

    // GET /node/bob?mask=alice&stub_hidden=true — bob is hidden from the mask.
    let req = get("/node/bob?mask=alice&stub_hidden=true");
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "hidden key in stub mode must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    // Assert FULL body shape: only key and restricted, nothing else.
    let obj = v.as_object().expect("response must be a JSON object");
    assert_eq!(
        obj.len(),
        2,
        "stub JSON must contain exactly 2 fields (key, restricted): {v}"
    );
    assert_eq!(v["key"], json!("bob"), "stub key must be the requested key");
    assert_eq!(v["restricted"], json!(true), "restricted must be true");
    assert!(v.get("label").is_none(), "stub must not leak label");
    assert!(v.get("props").is_none(), "stub must not leak props");

    // GET /node/alice?mask=alice — visible key returns normal info.
    let req2 = get("/node/alice?mask=alice&stub_hidden=true");
    let (status2, body2, _) = send(app.clone(), req2).await;
    assert_eq!(status2, StatusCode::OK);
    let v2 = parse_json(&body2);
    assert_eq!(v2["key"], json!("alice"));
    assert!(v2.get("label").is_some(), "visible node must have label");

    // GET /node/ghost?mask=alice&stub_hidden=true — absent key must be 404.
    let req3 = get("/node/ghost?mask=alice&stub_hidden=true");
    let (status3, _, _) = send(app, req3).await;
    assert_eq!(
        status3,
        StatusCode::NOT_FOUND,
        "absent key must still be 404 even in stub mode"
    );
}

/// GET /node/{key}/edges?mask=alice,carol&stub_hidden=true must include edges
/// to hidden neighbors rendered as `{"key": "<key>", "restricted": true}`.
#[tokio::test]
async fn client_mask_stub_node_edges_includes_restricted_endpoint() {
    let (app, db) = open("stub-node-edges-http");
    {
        let mut g = db.write();
        g.insert_node("P", "alice", vec![]).unwrap();
        g.insert_node("P", "bob", vec![]).unwrap(); // will be hidden
        g.insert_node("P", "carol", vec![]).unwrap();
        g.insert_edge("KNOWS", "alice", "bob").unwrap();
        g.insert_edge("KNOWS", "alice", "carol").unwrap();
    }

    // mask=alice,carol → bob is hidden; stub_hidden=true → bob appears as stub endpoint.
    let req = get("/node/alice/edges?mask=alice,carol&stub_hidden=true");
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "node edges stub request must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    let edges = v["edges"]
        .as_array()
        .expect("response must have edges array");
    assert_eq!(edges.len(), 2, "both edges must appear in stub mode: {v}");

    // Find the edge where bob appears as dst_key restricted stub.
    let bob_edge = edges.iter().find(|e| {
        e["dst_key"].is_object()
            && e["dst_key"]["key"] == json!("bob")
            && e["dst_key"]["restricted"] == json!(true)
    });
    assert!(
        bob_edge.is_some(),
        "bob must appear as restricted stub dst_key: {v}"
    );

    // Carol edge must have dst_key as plain string.
    let carol_edge = edges.iter().find(|e| e["dst_key"] == json!("carol"));
    assert!(
        carol_edge.is_some(),
        "carol must appear as plain string: {v}"
    );
}

/// A role-token request with `stub_hidden: true` in the body must NOT produce
/// stubs — the role path is hard-coded to Omit mode and the flag is ignored.
///
/// The test inserts a visible (Pub) and a hidden (Secret) node, issues the
/// query from a role token that only sees Pub nodes, and verifies that Secret
/// never appears in any form (no key, no restricted field).
#[tokio::test]
async fn role_token_stub_hidden_flag_is_ignored() {
    let (app, db) = open_rbac(
        "stub-role-ignored",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // Role token with stub_hidden: true.  sec1 must NOT appear.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({
            "cypher": "MATCH (n) RETURN n.id",
            "stub_hidden": true,
        }),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "role token stub_hidden request must be 200"
    );
    let v = parse_json(&body);
    let body_str = v.to_string();
    assert!(
        !body_str.contains("sec1"),
        "sec1 must not appear in any form in role-token response: {v}"
    );
    assert!(
        !body_str.contains("restricted"),
        "restricted field must not appear in role-token response: {v}"
    );

    // Verify pub1 is present.
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "role token must see exactly 1 node (pub1): {v}"
    );
}

/// `stub_hidden: true` on the role-token path also must not produce stubs
/// when a client-supplied mask is present (role mask + client mask intersection
/// stays Omit).
#[tokio::test]
async fn role_token_with_client_mask_and_stub_hidden_gets_no_stubs() {
    let (app, db) = open_rbac(
        "stub-role-client-mask",
        &[("analyst", &["Pub"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    db.write().insert_node("Pub", "pub1", vec![]).unwrap();
    db.write().insert_node("Pub", "pub2", vec![]).unwrap();
    db.write().insert_node("Secret", "sec1", vec![]).unwrap();

    // Role token with a client mask that includes pub1 only + stub_hidden: true.
    // sec1 is outside both the role mask and the client mask — no stub must appear.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({
            "cypher": "MATCH (n) RETURN n.id",
            "mask": ["pub1", "sec1"],
            "stub_hidden": true,
        }),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    let body_str = v.to_string();
    assert!(
        !body_str.contains("sec1"),
        "sec1 must not appear even with stub_hidden on role path: {v}"
    );
    assert!(
        !body_str.contains("restricted"),
        "restricted field must not appear on role path: {v}"
    );
}

/// Full-token GET /neighborhood/{key}?mask=...&stub_hidden=true must return
/// hidden direct neighbours as stub rows (key present, label null).
/// The same request without stub_hidden must omit the hidden neighbours entirely.
///
/// Graph: alice -KNOWS-> bob (hidden from mask) -KNOWS-> carol (visible)
/// Mask: {alice, carol}
///
/// stub_hidden=true  → alice's neighborhood at depth=1 includes bob as stub row
/// stub_hidden absent → alice's neighborhood at depth=1 is empty (bob hidden)
///
/// In both cases carol must NOT appear (BFS doesn't expand through hidden bob).
#[tokio::test]
async fn client_mask_stub_neighborhood_shows_hidden_direct_neighbor() {
    let (app, db) = open("stub-nb-http");
    {
        let mut g = db.write();
        g.insert_node("P", "alice", vec![]).unwrap();
        g.insert_node("P", "bob", vec![]).unwrap(); // hidden from mask
        g.insert_node("P", "carol", vec![]).unwrap();
        g.insert_edge("KNOWS", "alice", "bob").unwrap();
        g.insert_edge("KNOWS", "bob", "carol").unwrap();
    }

    // Stub mode: bob appears as a stub row (label absent/null); carol does not.
    let req = get("/node/alice/neighborhood?mask=alice,carol&stub_hidden=true&depth=2&dir=out");
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "stub neighborhood must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    let rows = v["rows"].as_array().expect("must have rows");

    let bob_stub_row = rows.iter().find(|r| {
        let row = r.as_array().unwrap();
        row.first() == Some(&json!("bob")) && row.get(1) == Some(&json!(null))
    });
    assert!(
        bob_stub_row.is_some(),
        "stub mode: bob must appear as stub row (key=bob, label=null): {v}"
    );

    let carol_row = rows.iter().find(|r| {
        r.as_array()
            .and_then(|row| row.first())
            .map(|k| k == &json!("carol"))
            .unwrap_or(false)
    });
    assert!(
        carol_row.is_none(),
        "carol must NOT appear — BFS must not expand through hidden bob: {v}"
    );

    // Omit mode (no stub_hidden): bob is invisible, neighborhood is empty.
    let req2 = get("/node/alice/neighborhood?mask=alice,carol&depth=2&dir=out");
    let (status2, body2, _) = send(app, req2).await;
    assert_eq!(status2, StatusCode::OK);
    let v2 = parse_json(&body2);
    let rows2 = v2["rows"].as_array().expect("must have rows");
    assert_eq!(
        rows2.len(),
        0,
        "omit mode: neighborhood must be empty when hidden node blocks all paths: {v2}"
    );
}

// ── HTTP: POST /nodes/{key}/rename ────────────────────────────────────────────

#[tokio::test]
async fn http_rename_node_success() {
    let (app, db) = open("http-rename-ok");
    seed_person(&db, "alice");

    let req = json_req("POST", "/nodes/alice/rename", json!({"new_key": "alice2"}));
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rename must return 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(v["ok"], json!(true));

    // Old key must be gone; new key must exist.
    let (s2, _b2, _) = send(app.clone(), get("/node/alice")).await;
    assert_eq!(s2, StatusCode::NOT_FOUND, "alice must be gone after rename");

    let (s3, b3, _) = send(app, get("/node/alice2")).await;
    assert_eq!(
        s3,
        StatusCode::OK,
        "alice2 must exist after rename: {}",
        String::from_utf8_lossy(&b3)
    );
}

#[tokio::test]
async fn http_rename_node_404_when_not_found() {
    let (app, _db) = open("http-rename-404");

    let req = json_req("POST", "/nodes/ghost/rename", json!({"new_key": "ghost2"}));
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "rename of non-existent key must be 404: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn http_rename_node_409_when_target_exists() {
    let (app, db) = open("http-rename-409");
    seed_person(&db, "alice");
    seed_person(&db, "alice2");

    let req = json_req("POST", "/nodes/alice/rename", json!({"new_key": "alice2"}));
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "rename to existing key must be 409: {}",
        String::from_utf8_lossy(&body)
    );
}

// ── HTTP: POST /edges/upsert ──────────────────────────────────────────────────

#[tokio::test]
async fn http_upsert_edge_creates_missing_endpoints_and_edge() {
    let (app, _db) = open("http-upsert-ok");

    let req = json_req(
        "POST",
        "/edges/upsert",
        json!({
            "edge_type": "KNOWS",
            "src_key": "p1",
            "dst_key": "p2",
            "placeholder_label": "Person"
        }),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "upsert must succeed: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(
        v["nodes_created"],
        json!(2),
        "both endpoints must be created"
    );
    assert_eq!(v["edge_inserted"], json!(true));

    // Both nodes must now exist.
    let (s1, _, _) = send(app.clone(), get("/node/p1")).await;
    assert_eq!(s1, StatusCode::OK, "p1 must exist");
    let (s2, _, _) = send(app, get("/node/p2")).await;
    assert_eq!(s2, StatusCode::OK, "p2 must exist");
}

#[tokio::test]
async fn http_upsert_edge_idempotent_when_edge_exists() {
    let (app, db) = open("http-upsert-idem");
    {
        let mut w = db.write();
        w.insert_node("Person", "a", vec![]).unwrap();
        w.insert_node("Person", "b", vec![]).unwrap();
        w.insert_edge("KNOWS", "a", "b").unwrap();
    }

    let req = json_req(
        "POST",
        "/edges/upsert",
        json!({
            "edge_type": "KNOWS",
            "src_key": "a",
            "dst_key": "b",
            "placeholder_label": "Person"
        }),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::OK, "idempotent upsert must be 200");
    let v = parse_json(&body);
    assert_eq!(v["nodes_created"], json!(0), "no new nodes when both exist");
    assert_eq!(
        v["edge_inserted"],
        json!(false),
        "edge already existed — edge_inserted must be false"
    );
}

// ── POST /backup ──────────────────────────────────────────────────────────────

/// C1b: POST /backup with a full-access token and a concurrent writer thread
/// must produce a clean, openable backup whose node set matches the source.
#[tokio::test]
async fn http_backup_live_serve_dest_opens_clean() {
    use std::sync::{Arc, Barrier};

    let src_dir = tmp("http-backup-src");
    // The backup dest is confined to the backup root (CWD by default), so place
    // it under the crate's `target/` dir rather than the system temp dir.
    let dst_dir = std::env::current_dir().unwrap().join(format!(
        "target/graphdb-http-backup-dst-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dst_dir);

    let db = SharedDb::open(&src_dir).unwrap();
    // Seed initial data.
    {
        let mut w = db.write();
        w.insert_node("Person", "alice", vec![]).unwrap();
        w.insert_node("Person", "bob", vec![]).unwrap();
    }

    let app = router_with_auth(db.clone(), Some("admin".into()));

    // Spawn a concurrent writer thread that races with the backup.
    let barrier = Arc::new(Barrier::new(2));
    let db_writer = db.clone();
    let barrier_clone = barrier.clone();
    let writer = std::thread::spawn(move || {
        barrier_clone.wait(); // release once main thread is about to send backup
        for i in 0..20u32 {
            let key = format!("concurrent-{i}");
            let _ = db_writer.write().insert_node("Person", &key, vec![]);
        }
    });

    barrier.wait(); // let writer start racing

    let req = authed_json_req(
        "POST",
        "/backup",
        "admin",
        json!({"dest": dst_dir.to_str().unwrap()}),
    );
    let (status, body, _) = send(app, req).await;
    writer.join().unwrap();

    assert_eq!(
        status,
        StatusCode::OK,
        "POST /backup must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    assert_eq!(v["verified"], json!(true), "backup must be verified");
    assert!(
        v["bytes"].as_u64().unwrap_or(0) > 0,
        "backup bytes must be > 0"
    );

    // The destination must open cleanly and contain at least the seeded nodes.
    let backup_db = SharedDb::open(&dst_dir).expect("backup dest must open");
    assert!(
        backup_db.read().has_node("alice"),
        "alice must be in backup"
    );
    assert!(backup_db.read().has_node("bob"), "bob must be in backup");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

/// C1b: POST /backup with a role-bound token must return 403.
#[tokio::test]
async fn http_backup_role_token_is_forbidden() {
    let (app, _db) = open_rbac(
        "http-backup-role-403",
        &[("analyst", &["Person"], &[])],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/backup",
        "role-tok",
        json!({"dest": "/tmp/should-not-exist"}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "role token must be 403 on POST /backup"
    );
}

// ── T3: Scoped role writes over HTTP ─────────────────────────────────────────
//
// Decision table (plan §3): CREATE-class, UPDATE/DELETE-class, EDGE-CREATE.
// §4.3 error bodies are asserted verbatim throughout.
//
// Endpoint × outcome matrix:
//
//  Endpoint          | allow | scope-deny | vis-deny | write:None
//  ------------------+-------+------------+----------+-----------
//  POST /nodes       |  yes  |    yes     |   yes    |   yes
//  DELETE /node      |  yes  |    yes     |   yes    |   yes
//  POST /edges       |  yes  |    yes     |   yes    |   yes
//  DELETE /edges     |  yes  |    yes     |   n/a    |   yes
//  POST /edges/upsert|  yes  |    yes     |   yes    |   yes
//  PUT  /node/prop   |  yes  |    yes     |   yes    |   yes
//  DELETE /node/prop |  yes  |    n/a     |   yes    |   yes
//  POST /query write |  yes  |    yes     |   yes    |   yes
//  POST /ingest      |  yes  |    yes     |   n/a    |   yes
//
// Cross-cutting:
//  - rename/backup/stats/subscribe stay 403 for write-scoped roles
//  - concurrent role writers FIFO-serialize
//  - ingest all-or-nothing (edge failure rolls back entire request)
//  - ingest without create_labels → 403 naming missing scope (§7.3)

/// Helper: open a DB with write-scoped roles.
///
/// `roles` entries: (role_name, visible_labels, write_scope or None).
fn open_rbac_write(
    name: &str,
    roles: &[(&str, &[&str], Option<WriteScope>)],
    full_token: Option<&str>,
    role_token_map: &[(&str, &str)],
) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    let schema = Schema {
        roles: roles
            .iter()
            .map(|(rname, labels, write)| RoleDef {
                name: rname.to_string(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
                keys: vec![],
                write: write.clone(),
            })
            .collect(),
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();
    let rtoks: std::collections::HashMap<String, String> = role_token_map
        .iter()
        .map(|(tok, role)| (tok.to_string(), role.to_string()))
        .collect();
    let app = router_with_role_tokens(db.clone(), full_token.map(str::to_string), rtoks);
    (app, db)
}

/// Helper: agent role that can create/update/delete AgentNote nodes and RECALLS edges.
fn agent_write_scope() -> WriteScope {
    WriteScope {
        create_labels: vec!["AgentNote".into()],
        update_labels: vec!["AgentNote".into()],
        delete_labels: vec!["AgentNote".into()],
        create_edge_types: vec!["RECALLS".into()],
        delete_edge_types: vec!["RECALLS".into()],
    }
}

/// Authenticated DELETE with optional JSON body (for prop endpoints) or no body.
fn authed_delete(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

/// Authenticated PUT with JSON body.
fn authed_put_json(uri: &str, token: &str, body: Json) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ── POST /nodes — create_node ─────────────────────────────────────────────────

#[tokio::test]
async fn scoped_create_node_allowed() {
    let (app, db) = open_rbac_write(
        "t3-cn-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/nodes",
        "role-tok",
        json!({"label": "AgentNote", "key": "note1", "props": {}}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped create must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    // Effect is visible to the role's own read view.
    assert!(db.read().has_node("note1"), "note1 must exist after create");
}

#[tokio::test]
async fn scoped_create_node_scope_denied() {
    let (app, _db) = open_rbac_write(
        "t3-cn-scope",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    // "Secret" is not in create_labels — must get §4.3 scope-denied body.
    let req = authed_json_req(
        "POST",
        "/nodes",
        "role-tok",
        json!({"label": "Secret", "key": "evil", "props": {}}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'Secret' not in write scope (create_labels)"),
        "scope-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_create_node_vis_denied() {
    let (app, db) = open_rbac_write(
        "t3-cn-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    // Seed a hidden node (label "Secret" is outside the role's read mask).
    db.write()
        .insert_node("Secret", "hidden-key", vec![])
        .unwrap();
    // Role tries to create AgentNote with the same key — hidden collision → not-visible.
    let req = authed_json_req(
        "POST",
        "/nodes",
        "role-tok",
        json!({"label": "AgentNote", "key": "hidden-key", "props": {}}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: target node not visible"),
        "hidden-collision body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn write_none_create_node_endpoint_not_permitted() {
    // write:None role → v1 byte-identical body "writes are not permitted".
    let (app, _db) = open_rbac_write(
        "t3-cn-none",
        &[("analyst", &["AgentNote"], None)],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/nodes",
        "role-tok",
        json!({"label": "AgentNote", "key": "x", "props": {}}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: writes are not permitted"),
        "write:None must get v1 blanket-403 body: {v}"
    );
}

// ── DELETE /node/{key} — delete_node ─────────────────────────────────────────

#[tokio::test]
async fn scoped_delete_node_allowed() {
    let (app, db) = open_rbac_write(
        "t3-dn-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("AgentNote", "del-me", vec![])
        .unwrap();
    let (status, _, _) = send(app, authed_delete("/node/del-me", "role-tok")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!db.read().has_node("del-me"), "node must be deleted");
}

#[tokio::test]
async fn scoped_delete_node_scope_denied() {
    // Role has AgentNote in labels (visible) but NOT in delete_labels.
    let (app, db) = open_rbac_write(
        "t3-dn-scope",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec![], // no delete scope
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "nd1", vec![]).unwrap();
    let (status, body, _) = send(app, authed_delete("/node/nd1", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AgentNote' not in write scope (delete_labels)"),
        "delete scope-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_delete_node_vis_denied() {
    let (app, db) = open_rbac_write(
        "t3-dn-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("Secret", "hidden", vec![]).unwrap();
    let (status, body, _) = send(app, authed_delete("/node/hidden", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: target node not visible"),
        "vis-denied body must match §4.3: {v}"
    );
}

// ── POST /edges — create_edge ─────────────────────────────────────────────────

#[tokio::test]
async fn scoped_create_edge_allowed() {
    let (app, db) = open_rbac_write(
        "t3-ce-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "a", vec![]).unwrap();
    db.write().insert_node("AgentNote", "b", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/edges",
        "role-tok",
        json!({"type": "RECALLS", "src": "a", "dst": "b"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped edge create must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_create_edge_type_denied() {
    let (app, db) = open_rbac_write(
        "t3-ce-type",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "a", vec![]).unwrap();
    db.write().insert_node("AgentNote", "b", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/edges",
        "role-tok",
        json!({"type": "UNKNOWN_TYPE", "src": "a", "dst": "b"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: edge type 'UNKNOWN_TYPE' not in write scope (create_edge_types)"),
        "edge type-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_create_edge_endpoint_hidden() {
    let (app, db) = open_rbac_write(
        "t3-ce-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "src", vec![]).unwrap();
    db.write()
        .insert_node("Secret", "hidden-dst", vec![])
        .unwrap();
    let req = authed_json_req(
        "POST",
        "/edges",
        "role-tok",
        json!({"type": "RECALLS", "src": "src", "dst": "hidden-dst"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: edge endpoint not visible"),
        "edge endpoint vis-denied body must match §4.3: {v}"
    );
}

// ── DELETE /edges/{etype}/{src}/{dst} — delete_edge ──────────────────────────

#[tokio::test]
async fn scoped_delete_edge_allowed() {
    let (app, db) = open_rbac_write(
        "t3-de-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "x", vec![]).unwrap();
    db.write().insert_node("AgentNote", "y", vec![]).unwrap();
    db.write().insert_edge("RECALLS", "x", "y").unwrap();
    let (status, body, _) = send(app, authed_delete("/edges/RECALLS/x/y", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped delete edge must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_delete_edge_type_denied() {
    let (app, db) = open_rbac_write(
        "t3-de-type",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "x", vec![]).unwrap();
    db.write().insert_node("AgentNote", "y", vec![]).unwrap();
    db.write().insert_edge("OTHER_TYPE", "x", "y").unwrap();
    let (status, body, _) = send(app, authed_delete("/edges/OTHER_TYPE/x/y", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: edge type 'OTHER_TYPE' not in write scope (delete_edge_types)"),
        "delete edge type-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_delete_edge_hidden_endpoint() {
    // DELETE /edges with one hidden endpoint → "edge endpoint not visible".
    let (app, db) = open_rbac_write(
        "t3-de-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "src", vec![]).unwrap();
    // dst is a "Secret" node — hidden from role's mask.
    db.write()
        .insert_node("Secret", "hidden-dst", vec![])
        .unwrap();
    db.write()
        .insert_edge("RECALLS", "src", "hidden-dst")
        .unwrap();
    let (status, body, _) = send(
        app,
        authed_delete("/edges/RECALLS/src/hidden-dst", "role-tok"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: edge endpoint not visible"),
        "delete edge with hidden endpoint must match §4.3: {v}"
    );
}

// ── POST /edges/upsert — upsert_edge ─────────────────────────────────────────

#[tokio::test]
async fn scoped_upsert_edge_allowed() {
    let (app, db) = open_rbac_write(
        "t3-ue-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "src", vec![]).unwrap();
    db.write().insert_node("AgentNote", "dst", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/edges/upsert",
        "role-tok",
        json!({"edge_type": "RECALLS", "src_key": "src", "dst_key": "dst",
               "placeholder_label": "AgentNote"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped upsert edge must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_upsert_edge_type_denied() {
    let (app, db) = open_rbac_write(
        "t3-ue-type",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "src", vec![]).unwrap();
    db.write().insert_node("AgentNote", "dst", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/edges/upsert",
        "role-tok",
        json!({"edge_type": "FORBIDDEN_TYPE", "src_key": "src", "dst_key": "dst",
               "placeholder_label": "AgentNote"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!(
            "role-bound token: edge type 'FORBIDDEN_TYPE' not in write scope (create_edge_types)"
        ),
        "upsert edge type-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_upsert_edge_hidden_endpoint() {
    let (app, db) = open_rbac_write(
        "t3-ue-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "src", vec![]).unwrap();
    // dst exists but is a "Secret" node — hidden from role
    db.write()
        .insert_node("Secret", "hidden-dst", vec![])
        .unwrap();
    let req = authed_json_req(
        "POST",
        "/edges/upsert",
        "role-tok",
        json!({"edge_type": "RECALLS", "src_key": "src", "dst_key": "hidden-dst",
               "placeholder_label": "AgentNote"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: edge endpoint not visible"),
        "upsert endpoint vis-denied body must match §4.3: {v}"
    );
}

// ── PUT /node/{key}/prop/{field} — set_node_prop ─────────────────────────────

#[tokio::test]
async fn scoped_set_prop_allowed() {
    let (app, db) = open_rbac_write(
        "t3-sp-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "n1", vec![]).unwrap();
    let req = authed_put_json("/node/n1/prop/score", "role-tok", json!({"value": 42}));
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped set prop must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_set_prop_scope_denied() {
    // update_labels is empty — cannot SET any property.
    let (app, db) = open_rbac_write(
        "t3-sp-scope",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec![], // no update scope
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "n1", vec![]).unwrap();
    let req = authed_put_json("/node/n1/prop/score", "role-tok", json!({"value": 1}));
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AgentNote' not in write scope (update_labels)"),
        "set prop scope-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_set_prop_vis_denied() {
    let (app, db) = open_rbac_write(
        "t3-sp-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("Secret", "hid", vec![]).unwrap();
    let req = authed_put_json("/node/hid/prop/x", "role-tok", json!({"value": 1}));
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: target node not visible"),
        "set prop vis-denied body must match §4.3: {v}"
    );
}

// ── DELETE /node/{key}/prop/{field} — remove_node_prop ───────────────────────

#[tokio::test]
async fn scoped_remove_prop_allowed() {
    let (app, db) = open_rbac_write(
        "t3-rp-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("AgentNote", "n1", vec![("score".into(), Value::Int(5))])
        .unwrap();
    let (status, body, _) = send(app, authed_delete("/node/n1/prop/score", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped remove prop must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_remove_prop_vis_denied() {
    let (app, db) = open_rbac_write(
        "t3-rp-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("Secret", "hid", vec![("x".into(), Value::Int(1))])
        .unwrap();
    let (status, body, _) = send(app, authed_delete("/node/hid/prop/x", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: target node not visible"),
        "remove prop vis-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_remove_prop_scope_denied() {
    // DELETE /node/{key}/prop/{field} on a visible node whose label is NOT in
    // update_labels → §4.3 scope-denied body.
    let (app, db) = open_rbac_write(
        "t3-rp-scope",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec![], // no update scope
                delete_labels: vec!["AgentNote".into()],
                create_edge_types: vec!["RECALLS".into()],
                delete_edge_types: vec!["RECALLS".into()],
            }),
        )],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("AgentNote", "n1", vec![("x".into(), Value::Int(1))])
        .unwrap();
    let (status, body, _) = send(app, authed_delete("/node/n1/prop/x", "role-tok")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AgentNote' not in write scope (update_labels)"),
        "remove prop scope-denied body must match §4.3: {v}"
    );
}

// ── POST /query (write Cypher) ────────────────────────────────────────────────

#[tokio::test]
async fn scoped_query_create_allowed() {
    let (app, db) = open_rbac_write(
        "t3-qc-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "CREATE (n:AgentNote {id: 'q-create-1'})"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped query CREATE must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(db.read().has_node("q-create-1"), "node must be created");
}

#[tokio::test]
async fn scoped_query_create_scope_denied() {
    let (app, _db) = open_rbac_write(
        "t3-qc-scope",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "CREATE (n:AdminLabel {id: 'evil'})"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AdminLabel' not in write scope (create_labels)"),
        "query create scope-denied body must match §4.3: {v}"
    );
}

#[tokio::test]
async fn scoped_query_set_allowed() {
    let (app, db) = open_rbac_write(
        "t3-qs-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "n1", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "MATCH (n {id: 'n1'}) SET n.x = 1"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped query SET must be 200: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn scoped_query_set_vis_denied() {
    // The MATCH read phase is masked for role-scoped writes: a hidden node is
    // invisible to MATCH → zero rows → no SetProp op → 200 zero-rows.
    // This closes the existence oracle: hidden ≡ absent (spec §3.1).
    let (app, db) = open_rbac_write(
        "t3-qs-vis",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("Secret", "hid", vec![("x".into(), Value::Int(0))])
        .unwrap();
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "MATCH (n {id: 'hid'}) SET n.x = 1"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query SET on hidden node must be 200 zero-rows (hidden ≡ absent): {}",
        String::from_utf8_lossy(&body)
    );
    // Verify the hidden node is untouched (full-token read).
    let val = db.read().get_prop("hid", "x");
    assert_eq!(
        val,
        Some(Value::Int(0)),
        "hidden node prop must be unchanged after masked MATCH SET"
    );
}

#[tokio::test]
async fn scoped_query_write_none_endpoint_not_permitted() {
    // write:None role trying a write Cypher query → v1 byte-identical body.
    let (app, _db) = open_rbac_write(
        "t3-qw-none",
        &[("analyst", &["AgentNote"], None)],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "role-tok",
        json!({"cypher": "CREATE (n:AgentNote {id: 'x'})"}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: writes are not permitted"),
        "write:None query must get v1 blanket-403 body: {v}"
    );
}

// ── POST /ingest ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn scoped_ingest_allowed() {
    let (app, db) = open_rbac_write(
        "t3-ing-allow",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "AgentNote", "rows": [{"id": "note-a"}, {"id": "note-b"}]}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "scoped ingest must be 200: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(db.read().has_node("note-a"), "note-a must be ingested");
    assert!(db.read().has_node("note-b"), "note-b must be ingested");
}

#[tokio::test]
async fn scoped_ingest_label_not_in_scope_403() {
    // §7.3: /ingest requires create_labels to include the label.
    // "AdminLabel" is not in create_labels → §4.3 scope-denied.
    let (app, _db) = open_rbac_write(
        "t3-ing-scope",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "AdminLabel", "rows": [{"id": "x"}]}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ingest with unlisted label must be 403: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    // §7.3 ruling: the 403 names the missing scope (create_labels).
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AdminLabel' not in write scope (create_labels)"),
        "ingest scope-denied body must name the missing create_labels scope: {v}"
    );
}

#[tokio::test]
async fn scoped_ingest_no_create_labels_403() {
    // §7.3: role with empty create_labels cannot use /ingest at all.
    let (app, _db) = open_rbac_write(
        "t3-ing-nolabels",
        &[(
            "edge-only",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec![], // no create scope
                update_labels: vec![],
                delete_labels: vec![],
                create_edge_types: vec!["RECALLS".into()],
                delete_edge_types: vec![],
            }),
        )],
        Some("admin"),
        &[("role-tok", "edge-only")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "AgentNote", "rows": [{"id": "x"}]}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ingest with empty create_labels must be 403: {}",
        String::from_utf8_lossy(&body)
    );
    let v = parse_json(&body);
    // "AgentNote" not in empty create_labels → scope-denied naming the label.
    assert_eq!(
        v["error"],
        json!("role-bound token: label 'AgentNote' not in write scope (create_labels)"),
        "ingest no-create-labels body must name missing scope: {v}"
    );
}

#[tokio::test]
async fn scoped_ingest_with_write_none_role_403() {
    // write:None role → v1 byte-identical body "writes are not permitted".
    let (app, _db) = open_rbac_write(
        "t3-ing-none",
        &[("analyst", &["AgentNote"], None)],
        Some("admin"),
        &[("role-tok", "analyst")],
    );
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({"label": "AgentNote", "rows": [{"id": "x"}]}),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let v = parse_json(&body);
    assert_eq!(
        v["error"],
        json!("role-bound token: writes are not permitted"),
        "write:None ingest must get v1 blanket-403 body: {v}"
    );
}

#[tokio::test]
async fn scoped_ingest_edges_all_or_nothing() {
    // §7.2: all-or-nothing semantics — if one edge fails, nothing is applied.
    // The node rows succeed but the second edge has a type not in scope.
    let (app, db) = open_rbac_write(
        "t3-ing-aon",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write()
        .insert_node("AgentNote", "existing-a", vec![])
        .unwrap();
    db.write()
        .insert_node("AgentNote", "existing-b", vec![])
        .unwrap();
    // Ingest with two edges: first valid, second invalid type.
    // Note: ingest edge format is {edge_type, src, dst}.
    let req = authed_json_req(
        "POST",
        "/ingest",
        "role-tok",
        json!({
            "label": "AgentNote",
            "rows": [{"id": "new-node"}],
            "edges": [
                {"edge_type": "RECALLS", "src": "existing-a", "dst": "existing-b"},
                {"edge_type": "FORBIDDEN_EDGE", "src": "existing-a", "dst": "existing-b"}
            ]
        }),
    );
    let (status, body, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ingest all-or-nothing: forbidden edge must reject entire request: {}",
        String::from_utf8_lossy(&body)
    );
    // new-node must NOT have been created (all-or-nothing).
    assert!(
        !db.read().has_node("new-node"),
        "new-node must NOT exist after rejected ingest"
    );
}

// ── Endpoints that STAY 403 for write-scoped roles ────────────────────────────

#[tokio::test]
async fn write_scoped_rename_stays_403() {
    let (app, db) = open_rbac_write(
        "t3-rename-403",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    db.write().insert_node("AgentNote", "old", vec![]).unwrap();
    let req = authed_json_req(
        "POST",
        "/nodes/old/rename",
        "role-tok",
        json!({"new_key": "new"}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "rename must stay 403 for write-scoped role"
    );
}

#[tokio::test]
async fn write_scoped_backup_stays_403() {
    let (app, _db) = open_rbac_write(
        "t3-backup-403",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_json_req(
        "POST",
        "/backup",
        "role-tok",
        json!({"dest": "/tmp/must-not-exist"}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "backup must stay 403 for write-scoped role"
    );
}

#[tokio::test]
async fn write_scoped_stats_stays_403() {
    let (app, _db) = open_rbac_write(
        "t3-stats-403",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let (status, _, _) = send(app, authed_get("/stats", "role-tok")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "stats must stay 403 for write-scoped role"
    );
}

#[tokio::test]
async fn write_scoped_subscribe_stays_403() {
    let (app, _db) = open_rbac_write(
        "t3-sub-403",
        &[("agent", &["AgentNote"], Some(agent_write_scope()))],
        Some("admin"),
        &[("role-tok", "agent")],
    );
    let req = authed_ws_upgrade("/subscribe", "role-tok");
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "subscribe must stay 403 for write-scoped role"
    );
}

// ── Concurrent role writers FIFO-serialize ─────────────────────────────────────

#[tokio::test]
async fn concurrent_role_writers_fifo_serialize() {
    // Two concurrent role-token submissions for different keys must both succeed
    // and FIFO-serialize through the drain queue (no lost writes).
    let db = SharedDb::open(&tmp("t3-conc-fifo")).unwrap();
    let schema = Schema {
        roles: vec![RoleDef {
            name: "agent".into(),
            labels: vec!["AgentNote".into()],
            keys: vec![],
            write: Some(agent_write_scope()),
        }],
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();

    // Submit two creates concurrently through the queue.
    let db1 = db.clone();
    let db2 = db.clone();
    let h1 = std::thread::spawn(move || {
        db1.submit_batch_authz(
            "agent".into(),
            vec![core_api::BatchOp::InsertNode {
                label: "AgentNote".into(),
                key: "conc-1".into(),
                props: vec![],
            }],
        )
    });
    let h2 = std::thread::spawn(move || {
        db2.submit_batch_authz(
            "agent".into(),
            vec![core_api::BatchOp::InsertNode {
                label: "AgentNote".into(),
                key: "conc-2".into(),
                props: vec![],
            }],
        )
    });
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    assert!(r1.is_ok(), "concurrent write 1 must succeed: {r1:?}");
    assert!(r2.is_ok(), "concurrent write 2 must succeed: {r2:?}");
    assert!(db.read().has_node("conc-1"), "conc-1 must exist");
    assert!(db.read().has_node("conc-2"), "conc-2 must exist");
}

/// POST /query with `as_of` runs a time-travel read; the current state is
/// unaffected and a write + as_of is rejected.
#[tokio::test]
async fn http_query_as_of_time_travel() {
    let db = SharedDb::open(&tmp("query-as-of")).unwrap();
    let app = router_with_auth(db.clone(), Some("adm".into()));

    // Three separate write commits.
    for id in ["a", "b", "c"] {
        let req = authed_json_req(
            "POST",
            "/query",
            "adm",
            json!({"cypher": format!("CREATE (n:N {{id: '{id}'}})")}),
        );
        let (status, _, _) = send(app.clone(), req).await;
        assert_eq!(status, StatusCode::OK);
    }

    // as_of commit 0 → only 'a' existed.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "adm",
        json!({"cypher": "MATCH (n:N) RETURN n", "as_of": 0}),
    );
    let (status, body, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse_json(&body);
    assert_eq!(
        v["rows"].as_array().unwrap().len(),
        1,
        "as_of 0 sees one node"
    );

    // No as_of → current state, three nodes.
    let req = authed_json_req(
        "POST",
        "/query?format=json",
        "adm",
        json!({"cypher": "MATCH (n:N) RETURN n"}),
    );
    let (_, body, _) = send(app.clone(), req).await;
    let v = parse_json(&body);
    assert_eq!(
        v["rows"].as_array().unwrap().len(),
        3,
        "current sees three nodes"
    );

    // as_of + write is rejected.
    let req = authed_json_req(
        "POST",
        "/query",
        "adm",
        json!({"cypher": "CREATE (x:N {id: 'z'})", "as_of": 0}),
    );
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "as_of write must be rejected"
    );
}

/// /metrics returns expected JSON shape after inserts.
#[tokio::test]
async fn metrics_endpoint_returns_counters() {
    let (app, db) = open("metrics-counters");

    // Insert 2 nodes + 1 edge so the counters are non-trivial.
    seed_person(&db, "alice");
    seed_person(&db, "bob");
    db.write().insert_edge("KNOWS", "alice", "bob").unwrap();

    let (status, body, _) = send(app, get("/metrics")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "expected 200, body={}",
        String::from_utf8_lossy(&body)
    );

    let v = parse_json(&body);
    assert_eq!(v["nodes_live"], json!(2), "nodes_live");
    assert_eq!(v["nodes_tombstoned"], json!(0), "nodes_tombstoned");
    assert_eq!(v["edges"], json!(1), "edges");
    assert!(
        v["commit_seq"].as_u64().unwrap_or(0) >= 2,
        "commit_seq must be >= 2 after two node inserts, got {}",
        v["commit_seq"]
    );
    assert!(
        v["wal_size_bytes"].as_u64().unwrap_or(0) > 0,
        "wal_size_bytes must be > 0, got {}",
        v["wal_size_bytes"]
    );
    assert!(v["uptime_s"].is_number(), "uptime_s must be a number");
    // rss_bytes may be null on unsupported platforms but must be present.
    assert!(
        v.get("rss_bytes").is_some(),
        "rss_bytes key must be present in response"
    );
    let sq = &v["slow_queries"];
    assert!(sq["threshold_ms"].is_number(), "slow_queries.threshold_ms");
    assert!(sq["count"].is_number(), "slow_queries.count");
    assert!(sq["last"].is_array(), "slow_queries.last");
}

/// Role-bound token receives 403 on /metrics (same as /stats).
#[tokio::test]
async fn metrics_role_token_is_forbidden() {
    let db = SharedDb::open(&tmp("metrics-role-403")).unwrap();
    let app = router_with_role_tokens(
        db,
        Some("adm".into()),
        std::collections::HashMap::from([("viewer".to_string(), "reader".to_string())]),
    );
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .header(axum::http::header::AUTHORIZATION, "Bearer viewer")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(app, req).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "role token must be 403 on /metrics"
    );
}
