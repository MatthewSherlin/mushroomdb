//! `/schema/indexes` and `/schema/constraints` (0.10 TY-11). The routes run the
//! same functions Cypher DDL does, so each answer is compared with
//! `SHOW INDEXES` / `SHOW CONSTRAINTS`, and each tier is refused as DDL
//! refuses it.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use mushroomdb::{schema::Schema, RoleDef, SharedDb, Value, WriteScope};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::path::PathBuf;
use tower::ServiceExt;

const ADM_KEY: &str = "adm-key-0123456789abcdef0123456789abcdef";
const WR_KEY: &str = "wr-key-0123456789abcdef0123456789abcdef";
const RD_KEY: &str = "rd-key-0123456789abcdef0123456789abcdef";

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-srv-schema-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn keys_file(name: &str) -> mushroomdb::AccessKeys {
    let entries = [
        (ADM_KEY, "admin", None),
        (WR_KEY, "write", None),
        (RD_KEY, "read", Some("writer")),
    ];
    let keys: Vec<Json> = entries
        .iter()
        .enumerate()
        .map(|(n, (token, tier, role))| {
            let mut entry = json!({
                "name": format!("key{n}"),
                "hash": mushroomdb::hash_token(token).unwrap(),
                "tier": tier,
            });
            if let Some(role) = role {
                entry["role"] = json!(role);
            }
            entry
        })
        .collect();
    let dir = tmp(&format!("keys-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("keys.json");
    std::fs::write(&path, json!({ "keys": keys }).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    mushroomdb::AccessKeys::load(&path).unwrap()
}

/// A store with role `writer` (label `P` only), behind env token `full`, role
/// token `rtok` and the keys above. A `P` node and a `Q` node exist.
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
    {
        let mut g = db.write();
        g.insert_node("P", "p1", vec![("a".into(), Value::Int(1))])
            .unwrap();
        g.insert_node("Q", "q1", vec![("a".into(), Value::Int(1))])
            .unwrap();
    }
    let tokens: HashMap<String, String> = [("rtok".to_string(), "writer".to_string())].into();
    let app =
        server::router_with_access_keys(db.clone(), Some("full".into()), tokens, keys_file(name));
    (app, db)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Json>,
) -> (StatusCode, Json) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header(axum::http::header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Json::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Json::Null)
    };
    (status, json)
}

/// `SHOW …` through `/query`, as the rows of objects the schema routes return.
async fn show_rows(app: &Router, token: &str, what: &str) -> Vec<Json> {
    let (status, rs) = call(
        app,
        "POST",
        "/query?format=json",
        token,
        Some(json!({ "cypher": format!("SHOW {what}") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rs}");
    let columns = rs["columns"].as_array().unwrap().clone();
    rs["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let obj: serde_json::Map<String, Json> = columns
                .iter()
                .zip(row.as_array().unwrap())
                .map(|(c, v)| (c.as_str().unwrap().to_string(), v.clone()))
                .collect();
            Json::Object(obj)
        })
        .collect()
}

#[tokio::test]
async fn test_schema_indexes_crud() {
    let (app, db) = open("idx");
    let body = json!({"label": "P", "property": "a", "kind": "range"});
    let (status, created) = call(&app, "POST", "/schema/indexes", "full", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "idx_P_a");
    assert_eq!(db.read().range_indexes().len(), 1);

    let (status, rows) = call(&app, "GET", "/schema/indexes", "full", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = rows.as_array().unwrap().clone();
    assert_eq!(listed, show_rows(&app, "full", "INDEXES").await);
    assert_eq!(
        listed,
        vec![
            json!({"name": "idx_P_a", "type": "range", "label": "P", "property": "a", "state": "online"})
        ]
    );

    let (status, _) = call(&app, "DELETE", "/schema/indexes/idx_P_a", "full", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(db.read().range_indexes().is_empty());
    let (_, rows) = call(&app, "GET", "/schema/indexes", "full", None).await;
    assert_eq!(rows, json!([]));
}

#[tokio::test]
async fn named_range_and_equality_indexes_are_listed_with_every_kind() {
    let (app, _db) = open("kinds");
    let (status, created) = call(
        &app,
        "POST",
        "/schema/indexes",
        "full",
        Some(json!({"name": "by_a", "label": "P", "property": "a", "kind": "range"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, created) = call(
        &app,
        "POST",
        "/schema/indexes",
        "full",
        Some(json!({"label": "Q", "property": "a", "kind": "equality"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["name"], "eq_Q_a");
    let (_, rows) = call(&app, "GET", "/schema/indexes", "full", None).await;
    let types: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, vec!["range", "equality"]);
    assert_eq!(
        rows.as_array().unwrap().clone(),
        show_rows(&app, "full", "INDEXES").await
    );
    // The equality index is dropped by its listed name.
    let (status, _) = call(&app, "DELETE", "/schema/indexes/eq_Q_a", "full", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_schema_constraints_crud() {
    let (app, db) = open("con");
    let body = json!({"name": "uc1", "label": "P", "property": "email", "type": "unique"});
    let (status, created) = call(&app, "POST", "/schema/constraints", "full", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(db.read().constraints().len(), 1);
    let (status, rows) = call(&app, "GET", "/schema/constraints", "full", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rows,
        json!([{"name": "uc1", "type": "unique", "label": "P", "property": "email"}])
    );
    assert_eq!(
        rows.as_array().unwrap().clone(),
        show_rows(&app, "full", "CONSTRAINTS").await
    );
    // Its backing equality index is listed, and cannot be dropped under it.
    let (status, err) = call(&app, "DELETE", "/schema/indexes/eq_P_email", "full", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(err["error"].as_str().unwrap().contains("uc1"), "{err}");
    let (status, _) = call(&app, "DELETE", "/schema/constraints/uc1", "full", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(db.read().constraints().is_empty());
}

#[tokio::test]
async fn errors_map_to_400_404_409() {
    let (app, _db) = open("errs");
    let idx = |body: Json| call(&app, "POST", "/schema/indexes", "full", Some(body));
    // 400: reserved prefix, empty, bad kind, unknown kind, bad name, equality name mismatch.
    for body in [
        json!({"name": "eq_x", "label": "P", "property": "a", "kind": "range"}),
        json!({"name": "ft_x", "label": "P", "property": "a", "kind": "range"}),
        json!({"label": "", "property": "a", "kind": "range"}),
        json!({"label": "P", "kind": "range"}),
        json!({"label": "P", "property": "a", "kind": "fulltext"}),
        json!({"label": "P", "property": "a", "kind": 3}),
        json!({"name": "has space", "label": "P", "property": "a", "kind": "range"}),
        json!({"name": "other", "label": "P", "property": "a", "kind": "equality"}),
    ] {
        let (status, err) = idx(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {err}");
    }
    let (status, err) = idx(json!({"label": "P", "property": "a", "kind": "fulltext"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("range"), "{err}");

    let good = json!({"name": "i1", "label": "P", "property": "a", "kind": "range"});
    assert_eq!(idx(good.clone()).await.0, StatusCode::CREATED);
    assert_eq!(idx(good).await.0, StatusCode::CONFLICT);
    // The pair is taken under another name.
    let (status, _) =
        idx(json!({"name": "i2", "label": "P", "property": "a", "kind": "range"})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    assert_eq!(
        call(&app, "DELETE", "/schema/indexes/nope", "full", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, "DELETE", "/schema/constraints/nope", "full", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let c = json!({"name": "c1", "label": "P", "property": "b", "type": "unique"});
    let con = |body: Json| call(&app, "POST", "/schema/constraints", "full", Some(body));
    assert_eq!(con(c.clone()).await.0, StatusCode::CREATED);
    assert_eq!(con(c).await.0, StatusCode::CONFLICT);
    for body in [
        json!({"name": "c2", "label": "P", "property": "b", "type": "exists"}),
        json!({"label": "P", "property": "b", "type": "unique"}),
        json!({"name": "c2", "property": "b", "type": "unique"}),
    ] {
        let (status, err) = con(body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {err}");
    }
    // Existing duplicates make the constraint a conflict with stored state.
    {
        let mut g = _db.write();
        g.insert_node("P", "p2", vec![("d".into(), Value::Int(1))])
            .unwrap();
        g.insert_node("P", "p3", vec![("d".into(), Value::Int(1))])
            .unwrap();
    }
    let (status, err) =
        con(json!({"name": "c3", "label": "P", "property": "d", "type": "unique"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
}

#[tokio::test]
async fn write_tier_writes_and_read_tier_and_role_tokens_cannot() {
    let (app, db) = open("tiers");
    seed_q_index(&db);
    let body = json!({"label": "P", "property": "a", "kind": "range"});
    // admin key and write key create; the write key also drops.
    let (status, _) = call(&app, "POST", "/schema/indexes", ADM_KEY, Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&app, "DELETE", "/schema/indexes/idx_P_a", WR_KEY, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, "POST", "/schema/indexes", WR_KEY, Some(body.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    let con = json!({"name": "c", "label": "P", "property": "b", "type": "unique"});
    assert_eq!(
        call(
            &app,
            "POST",
            "/schema/constraints",
            WR_KEY,
            Some(con.clone())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let before = db.read().range_indexes().len();
    // read key and role token: 403 on every write, nothing changes.
    for token in [RD_KEY, "rtok"] {
        for (method, uri, b) in [
            (
                "POST",
                "/schema/indexes",
                Some(json!({"name": "j", "label": "P", "property": "z", "kind": "range"})),
            ),
            ("DELETE", "/schema/indexes/idx_P_a", None),
            (
                "POST",
                "/schema/constraints",
                Some(json!({"name": "c9", "label": "P", "property": "y", "type": "unique"})),
            ),
            ("DELETE", "/schema/constraints/c", None),
        ] {
            let (status, err) = call(&app, method, uri, token, b).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{token} {method} {uri}: {err}"
            );
        }
    }
    assert_eq!(db.read().range_indexes().len(), before);
    assert_eq!(db.read().constraints().len(), 2, "qc and c");
    // no credential: 401.
    let req = Request::builder()
        .method("GET")
        .uri("/schema/indexes")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

fn seed_q_index(db: &SharedDb) {
    db.write()
        .query_write("CREATE INDEX q FOR (n:Q) ON (n.a)", &Default::default())
        .unwrap();
    db.write()
        .query_write(
            "CREATE CONSTRAINT qc FOR (n:Q) REQUIRE n.b IS UNIQUE",
            &Default::default(),
        )
        .unwrap();
}

#[tokio::test]
async fn gets_are_narrowed_for_roles_exactly_like_show() {
    let (app, db) = open("narrow");
    seed_q_index(&db);
    db.write()
        .query_write("CREATE INDEX i FOR (n:P) ON (n.a)", &Default::default())
        .unwrap();
    // Unscoped tokens see all of it; the role (token and read key) sees `P` only.
    for (token, labels) in [
        ("full", vec!["Q", "P", "Q"]),
        (WR_KEY, vec!["Q", "P", "Q"]),
        ("rtok", vec!["P"]),
        (RD_KEY, vec!["P"]),
    ] {
        let (status, rows) = call(&app, "GET", "/schema/indexes", token, None).await;
        assert_eq!(status, StatusCode::OK, "{token}");
        let got: Vec<&str> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["label"].as_str().unwrap())
            .collect();
        assert_eq!(got, labels, "{token}");
        assert_eq!(
            rows.as_array().unwrap().clone(),
            show_rows(&app, token, "INDEXES").await,
            "{token}"
        );
        let (status, rows) = call(&app, "GET", "/schema/constraints", token, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            rows.as_array().unwrap().clone(),
            show_rows(&app, token, "CONSTRAINTS").await,
            "{token}"
        );
    }
    let (_, rows) = call(&app, "GET", "/schema/constraints", "rtok", None).await;
    assert_eq!(rows, json!([]));
}
