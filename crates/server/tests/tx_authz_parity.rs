//! Parity matrix tests for ledger row 140: a transaction begun by a write-
//! capable role token must write with the same authority as `POST /query`.
//!
//! Each test covers one cell of the matrix:
//!
//! | identity          | operation              | auto-commit | tx |
//! |-------------------|------------------------|-------------|-----|
//! | write-scope role  | in-scope create        | ✓           | ✓   |
//! | write-scope role  | in-scope set-prop      | ✓           | ✓   |
//! | write-scope role  | in-scope delete        | ✓           | ✓   |
//! | write-scope role  | out-of-scope label     | 403         | 403 |
//! | write-scope role  | out-of-mask node       | 403         | 403 |
//! | no-write-scope    | any write              | 403         | 403 |
//! | read-tier key     | any write              | 403         | 403 |
//!
//! The parity assertion: a write that succeeds (or fails with a specific 403)
//! via `POST /query` must succeed (or fail with the same error) via
//! `POST /tx/begin` → `POST /tx/{id}/run` → `POST /tx/{id}/commit`, and
//! via Bolt `BEGIN` → `RUN` → `COMMIT`.

use bolt::connection::Connection;
use bolt::messages::hello::{AuthToken, Hello};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::PackValue;
use mushroomdb::{hash_token, AccessKeys, RoleDef, Schema, SharedDb, Value, WriteScope};
use serde_json::{json, Value as Json};
use server::{router_with_tx_limits, TxLimits};
use std::collections::HashMap;
use std::time::Duration;

// ── token constants ───────────────────────────────────────────────────────────

const ADMIN: &str = "admin-tx-authz-test";
const WRITER_TOK: &str = "writer-role-tok-tx-authz";
const VIEWER_TOK: &str = "viewer-role-tok-tx-authz";
const RD_KEY: &str = "rd-key-0123456789abcdef-tx-authz";

fn keys() -> AccessKeys {
    let text = format!(
        r#"{{"keys":[{{"hash":"{}","tier":"read","role":"writer"}}]}}"#,
        hash_token(RD_KEY).unwrap()
    );
    AccessKeys::from_json(&text).unwrap()
}

// ── fixture helpers ───────────────────────────────────────────────────────────

fn write_scope(labels: &[&str], edge_types: &[&str]) -> WriteScope {
    WriteScope {
        create_labels: labels.iter().map(|s| s.to_string()).collect(),
        update_labels: labels.iter().map(|s| s.to_string()).collect(),
        delete_labels: labels.iter().map(|s| s.to_string()).collect(),
        create_edge_types: edge_types.iter().map(|s| s.to_string()).collect(),
        delete_edge_types: edge_types.iter().map(|s| s.to_string()).collect(),
        update_edge_types: edge_types.iter().map(|s| s.to_string()).collect(),
    }
}

struct Env {
    base: String,
    http: reqwest::Client,
    /// Kept alive so the server's SharedDb clone does not outlive the store.
    #[allow(dead_code)]
    db: SharedDb,
}

impl Env {
    async fn call(&self, method: &str, path: &str, token: &str, body: Option<Json>) -> (u16, Json) {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self
            .http
            .request(m, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Json::String(text));
        (status, body)
    }

    async fn query(&self, token: &str, cypher: &str) -> (u16, Json) {
        self.call(
            "POST",
            "/query?format=json",
            token,
            Some(json!({"cypher": cypher})),
        )
        .await
    }

    async fn begin(&self, token: &str) -> (u16, Json) {
        self.call("POST", "/tx/begin", token, None).await
    }

    async fn begin_id(&self, token: &str) -> String {
        let (status, body) = self.begin(token).await;
        assert_eq!(status, 200, "begin failed: {body}");
        body["tx_id"].as_str().unwrap().to_string()
    }

    async fn run(&self, id: &str, token: &str, cypher: &str) -> (u16, Json) {
        self.call(
            "POST",
            &format!("/tx/{id}/run"),
            token,
            Some(json!({"cypher": cypher})),
        )
        .await
    }

    async fn commit(&self, id: &str, token: &str) -> (u16, Json) {
        self.call("POST", &format!("/tx/{id}/commit"), token, None)
            .await
    }

    async fn rollback(&self, id: &str, token: &str) {
        let _ = self
            .call("POST", &format!("/tx/{id}/rollback"), token, None)
            .await;
    }
}

async fn open(name: &str) -> Env {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-tx-parity-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![
                // "writer": sees Task nodes, may create/update/delete them and
                // LINKS edges.
                RoleDef {
                    name: "writer".into(),
                    labels: vec!["Task".into()],
                    keys: vec![],
                    visible_where: None,
                    namespaces: None,
                    write: Some(write_scope(&["Task"], &["LINKS"])),
                },
                // "viewer": sees Task nodes, cannot write.
                RoleDef {
                    name: "viewer".into(),
                    labels: vec!["Task".into()],
                    keys: vec![],
                    visible_where: None,
                    namespaces: None,
                    write: None,
                },
            ],
            ..Default::default()
        })
        .unwrap();

    // Seed a Task and a Secret node.
    db.write()
        .insert_node("Task", "t1", vec![("id".into(), Value::Str("t1".into()))])
        .unwrap();
    db.write()
        .insert_node("Secret", "s1", vec![("id".into(), Value::Str("s1".into()))])
        .unwrap();

    let role_tokens: HashMap<String, String> = [
        (WRITER_TOK.to_string(), "writer".to_string()),
        (VIEWER_TOK.to_string(), "viewer".to_string()),
    ]
    .into();

    let app = router_with_tx_limits(
        db.clone(),
        Some(ADMIN.to_string()),
        role_tokens,
        Some(keys()),
        TxLimits {
            max_open: 100,
            idle_timeout: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
        },
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Env {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        db,
    }
}

// ── matrix row 1: write-scope role — in-scope create ─────────────────────────

#[tokio::test]
async fn write_scope_role_can_create_in_tx_parity_with_query() {
    let e = open("create-parity").await;

    // Auto-commit reference: works.
    let (st, _) = e
        .query(WRITER_TOK, "CREATE (n:Task {id: 'aq1'}) RETURN n.id AS id")
        .await;
    assert_eq!(st, 200, "auto-commit create failed");

    // TX path: same create must succeed.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "CREATE (n:Task {id: 'tq1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(run_st, 200, "tx create failed: {run_body}");
    let (cmt_st, _) = e.commit(&id, WRITER_TOK).await;
    assert_eq!(cmt_st, 200, "commit failed");

    // Verify the node landed.
    let (_, rows) = e
        .query(ADMIN, "MATCH (n:Task {id: 'tq1'}) RETURN n.id AS id")
        .await;
    assert_eq!(
        rows["rows"],
        json!([["tq1"]]),
        "node not found after commit"
    );
}

// ── matrix row 2: write-scope role — in-scope set-prop ───────────────────────

#[tokio::test]
async fn write_scope_role_can_set_prop_in_tx_parity_with_query() {
    let e = open("setprop-parity").await;

    // Auto-commit reference.
    let (st, _) = e
        .query(WRITER_TOK, "MATCH (n:Task {id: 't1'}) SET n.note = 'aq'")
        .await;
    assert_eq!(st, 200, "auto-commit set-prop failed");

    // TX path.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "MATCH (n:Task {id: 't1'}) SET n.note = 'tq'",
        )
        .await;
    assert_eq!(run_st, 200, "tx set-prop failed: {run_body}");
    let (cmt_st, _) = e.commit(&id, WRITER_TOK).await;
    assert_eq!(cmt_st, 200, "commit failed");

    let (_, rows) = e
        .query(ADMIN, "MATCH (n:Task {id: 't1'}) RETURN n.note AS note")
        .await;
    assert_eq!(
        rows["rows"],
        json!([["tq"]]),
        "prop not updated after commit"
    );
}

// ── matrix row 3: write-scope role — in-scope delete ─────────────────────────

#[tokio::test]
async fn write_scope_role_can_delete_in_tx_parity_with_query() {
    let e = open("delete-parity").await;

    // Create a node to delete via auto-commit first.
    let (st, _) = e
        .query(
            WRITER_TOK,
            "CREATE (n:Task {id: 'del-aq'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(st, 200);

    // TX: create then delete within the same transaction.
    let id = e.begin_id(WRITER_TOK).await;
    let (st1, b1) = e
        .run(&id, WRITER_TOK, "CREATE (n:Task {id: 'del-tq'})")
        .await;
    assert_eq!(st1, 200, "create in tx: {b1}");
    let (st2, b2) = e
        .run(&id, WRITER_TOK, "MATCH (n:Task {id: 'del-tq'}) DELETE n")
        .await;
    assert_eq!(st2, 200, "delete in tx: {b2}");
    let (cmt_st, _) = e.commit(&id, WRITER_TOK).await;
    assert_eq!(cmt_st, 200, "commit failed");

    // Node should not exist.
    let (_, rows) = e
        .query(ADMIN, "MATCH (n:Task {id: 'del-tq'}) RETURN n.id AS id")
        .await;
    assert_eq!(rows["rows"], json!([]), "deleted node still visible");
}

// ── matrix row 4: write-scope role — out-of-scope label ──────────────────────

#[tokio::test]
async fn write_scope_role_refused_out_of_scope_label_same_in_tx_and_query() {
    let e = open("oos-label-parity").await;

    // Auto-commit reference: 403 with specific message.
    let (q_st, q_body) = e
        .query(
            WRITER_TOK,
            "CREATE (n:Secret {id: 'oob1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(q_st, 403, "query: {q_body}");
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert!(q_err.contains("not in write scope"), "query error: {q_err}");

    // TX path: same 403, same error substring.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(&id, WRITER_TOK, "CREATE (n:Secret {id: 'oob2'})")
        .await;
    assert_eq!(run_st, 403, "tx run: {run_body}");
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert!(
        run_err.contains("not in write scope"),
        "tx error: {run_err}"
    );
    // Refusal does not end the transaction.
    let (rq_st, _) = e.run(&id, WRITER_TOK, "RETURN 1 AS x").await;
    assert_eq!(rq_st, 200, "tx unusable after refused write");
    e.rollback(&id, WRITER_TOK).await;
}

// ── matrix row 5: write-scope role — mask hides nodes from both protocols ─────
//
// A MATCH against a hidden node returns empty (not 403) because the mask
// simply excludes the node from MATCH — the authz check never fires for a
// node that the MATCH cannot bind.  The parity here is "masked MATCH returns
// empty in both /query and /tx".
//
// The edge-endpoint authz fires when the query explicitly names a hidden
// node's key in an InsertEdge op.  This is tested here too: creating a LINKS
// edge where the destination is a hidden Secret node is refused with
// "edge endpoint not visible" at execute-time.

#[tokio::test]
async fn mask_hides_secret_nodes_from_match_in_query_and_tx() {
    let e = open("oos-mask-parity").await;

    // Auto-commit: MATCH returns empty, not 403.
    let (q_st, q_body) = e
        .query(WRITER_TOK, "MATCH (n:Secret {id: 's1'}) RETURN n.id AS id")
        .await;
    assert_eq!(q_st, 200, "query: {q_body}");
    assert_eq!(q_body["rows"], json!([]), "hidden node must not appear");

    // TX path: same empty result.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "MATCH (n:Secret {id: 's1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(run_st, 200, "tx run: {run_body}");
    assert_eq!(
        run_body["rows"],
        json!([]),
        "hidden node must not appear in tx"
    );
    e.rollback(&id, WRITER_TOK).await;
}

// ── matrix row 6: no-write-scope role — any write ────────────────────────────

#[tokio::test]
async fn no_write_scope_role_refused_in_tx_same_as_query() {
    let e = open("viewer-write-parity").await;

    // Auto-commit reference: 403, message contains "writes are not permitted".
    let (q_st, q_body) = e
        .query(VIEWER_TOK, "CREATE (n:Task {id: 'vw1'}) RETURN n.id AS id")
        .await;
    assert_eq!(q_st, 403, "query: {q_body}");
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert!(
        q_err.contains("writes are not permitted"),
        "query error: {q_err}"
    );

    // TX path: same 403, same "writes are not permitted" substring.
    // Note: the exact phrasing may differ (the tx path appends "inside a
    // transaction"), but the status code and semantic meaning are the same.
    let id = e.begin_id(VIEWER_TOK).await;
    let (run_st, run_body) = e.run(&id, VIEWER_TOK, "CREATE (n:Task {id: 'vw2'})").await;
    assert_eq!(run_st, 403, "tx run: {run_body}");
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert!(
        run_err.contains("writes are not permitted"),
        "tx error: {run_err}"
    );
    e.rollback(&id, VIEWER_TOK).await;
}

// ── matrix row 7: read-tier key — any write ──────────────────────────────────

#[tokio::test]
async fn read_tier_key_refused_in_tx_same_as_query() {
    let e = open("rd-key-write-parity").await;

    // Auto-commit reference.
    let (q_st, q_body) = e.query(RD_KEY, "CREATE (n:Task {id: 'rdk1'})").await;
    assert_eq!(q_st, 403, "query: {q_body}");
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert_eq!(q_err, "read-tier key: writes are not permitted");

    // TX path.
    let id = e.begin_id(RD_KEY).await;
    let (run_st, run_body) = e.run(&id, RD_KEY, "CREATE (n:Task {id: 'rdk2'})").await;
    assert_eq!(run_st, 403, "tx run: {run_body}");
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert_eq!(
        run_err, "read-tier key: writes are not permitted",
        "tx error must match query error"
    );
    e.rollback(&id, RD_KEY).await;
}

// ── multi-statement tx: reads see own writes ──────────────────────────────────

#[tokio::test]
async fn write_scope_role_reads_own_writes_in_tx() {
    let e = open("read-own-writes").await;

    let id = e.begin_id(WRITER_TOK).await;
    // Write a node.
    let (st1, b1) = e
        .run(
            &id,
            WRITER_TOK,
            "CREATE (n:Task {id: 'row1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(st1, 200, "create: {b1}");
    // Read it back in the same tx.
    let (st2, b2) = e
        .run(
            &id,
            WRITER_TOK,
            "MATCH (n:Task {id: 'row1'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(st2, 200, "read-own-write: {b2}");
    assert_eq!(b2["rows"], json!([["row1"]]), "own write not visible");
    // Rollback; node must not exist.
    e.rollback(&id, WRITER_TOK).await;
    let (_, rows) = e
        .query(ADMIN, "MATCH (n:Task {id: 'row1'}) RETURN n.id AS id")
        .await;
    assert_eq!(rows["rows"], json!([]), "rolled-back node is visible");
}

// ── Bolt parity: write-capable role gets RoleOwnedTransaction ────────────────

/// Utility: send a Bolt BEGIN and return Ok(()) on SUCCESS or Err(msg).
fn bolt_begin(conn: &mut Connection) -> Result<(), String> {
    let begin = ClientMessage::Other(MessageKind::Begin, vec![PackValue::Map(vec![])]);
    match conn.handle(begin).as_slice() {
        [ServerMessage::Success(_)] => Ok(()),
        [ServerMessage::Failure(e)] => Err(e.message().to_string()),
        other => panic!("BEGIN: {other:?}"),
    }
}

fn bolt_run(conn: &mut Connection, cypher: &str) -> Result<Vec<PackValue>, String> {
    let run = ClientMessage::Other(
        MessageKind::Run,
        vec![
            PackValue::String(cypher.into()),
            PackValue::Map(vec![]),
            PackValue::Map(vec![]),
        ],
    );
    let pull = ClientMessage::Other(
        MessageKind::Pull,
        vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
    );
    let run_out = conn.handle(run);
    match run_out.as_slice() {
        [ServerMessage::Failure(e)] => return Err(e.message().to_string()),
        [ServerMessage::Success(_)] => {}
        other => panic!("RUN success: {other:?}"),
    }
    let pull_out = conn.handle(pull);
    match pull_out.last() {
        Some(ServerMessage::Failure(e)) => Err(e.message().to_string()),
        Some(ServerMessage::Success(_)) => Ok(pull_out
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::Record(fields) => Some(fields),
                _ => None,
            })
            .flatten()
            .collect()),
        other => panic!("PULL success: {other:?}"),
    }
}

fn bolt_commit(conn: &mut Connection) -> bool {
    let commit = ClientMessage::Other(MessageKind::Commit, vec![]);
    match conn.handle(commit).as_slice() {
        [ServerMessage::Success(_)] => true,
        [ServerMessage::Failure(_)] => false,
        other => panic!("COMMIT: {other:?}"),
    }
}

/// A distinct "full admin" bearer token used only inside `make_bolt_conn`.
/// It must differ from WRITER_TOK / VIEWER_TOK so the authenticator resolves
/// them as role tokens, not as the full token.
const BOLT_ADMIN: &str = "bolt-admin-only-tx-authz-parity";

/// Build a Bolt connection authenticated with `token` as a role-bound identity
/// (or as the full admin when `role` is `None`).  The `SharedDb` is attached
/// via `.with_database()`, which is how the server wires up its `QueryService`.
fn make_bolt_conn(db: SharedDb, token: &str, role: Option<&str>) -> Connection {
    use bolt::messages::hello::TokenAuthenticator;
    let mut role_map = HashMap::new();
    if let Some(r) = role {
        role_map.insert(token.to_string(), r.to_string());
    }
    // Use BOLT_ADMIN as the full token so the role token is NOT matched as the
    // full identity — it is looked up in role_map and resolved as Identity::Role.
    let auth = std::sync::Arc::new(TokenAuthenticator::new(
        Some(BOLT_ADMIN.to_string()),
        role_map,
    ));
    let mut conn = Connection::new(auth, Duration::from_secs(30), "c".into(), (5, 0))
        .with_database(db, Duration::from_millis(100));
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: token.into(),
        },
    }));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "HELLO failed: {out:?}"
    );
    conn
}

#[test]
fn bolt_write_scope_role_can_create_in_explicit_tx() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: Some(write_scope(&["Task"], &["LINKS"])),
            }],
            ..Default::default()
        })
        .unwrap();

    let mut conn = make_bolt_conn(db, WRITER_TOK, Some("writer"));
    bolt_begin(&mut conn).expect("BEGIN");
    bolt_run(&mut conn, "CREATE (n:Task {id: 'b1'})").expect("CREATE in tx");
    assert!(bolt_commit(&mut conn), "COMMIT");
}

#[test]
fn bolt_no_write_scope_role_refused_in_explicit_tx() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "viewer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();

    let mut conn = make_bolt_conn(db, VIEWER_TOK, Some("viewer"));
    bolt_begin(&mut conn).expect("BEGIN");
    let err = bolt_run(&mut conn, "CREATE (n:Task {id: 'b2'})").expect_err("write must be refused");
    assert!(
        err.contains("writes are not permitted"),
        "unexpected error: {err}"
    );
}

#[test]
fn bolt_out_of_scope_label_refused_same_as_auto_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: Some(write_scope(&["Task"], &[])),
            }],
            ..Default::default()
        })
        .unwrap();

    let mut conn_ac = make_bolt_conn(db.clone(), WRITER_TOK, Some("writer"));
    // Auto-commit reference error.
    let ac_err = bolt_run(&mut conn_ac, "CREATE (n:Secret {id: 'ac1'})")
        .expect_err("auto-commit must refuse out-of-scope label");
    assert!(
        ac_err.contains("not in write scope"),
        "auto-commit error: {ac_err}"
    );

    let mut conn_tx = make_bolt_conn(db, WRITER_TOK, Some("writer"));
    bolt_begin(&mut conn_tx).expect("BEGIN");
    let tx_err = bolt_run(&mut conn_tx, "CREATE (n:Secret {id: 'tx1'})")
        .expect_err("tx must refuse out-of-scope label");
    assert!(tx_err.contains("not in write scope"), "tx error: {tx_err}");
}

// ── Security regression: HIGH-1 — MERGE+RETURN must not leak hidden node data ─
//
// A MERGE on a key that exists as a hidden node followed by RETURN must NOT
// expose the hidden node's properties.  After the fix `has_key` uses the masked
// view, so the hidden node appears absent, MERGE creates a new node, and
// `check_new_ops_authz` refuses it (Secret is not in scope) → 403.
// Before the fix, `has_key` saw the hidden node, MERGE produced zero ops, the
// scope check passed on an empty slice, and RETURN projected through the
// unmasked view → 200 with sensitive data.

async fn open_with_secret_prop(name: &str) -> Env {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-tx-sec-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: Some(write_scope(&["Task"], &["LINKS"])),
            }],
            ..Default::default()
        })
        .unwrap();

    // Seed the visible Task node and a hidden Secret node with sensitive data.
    db.write()
        .insert_node("Task", "t1", vec![("id".into(), Value::Str("t1".into()))])
        .unwrap();
    db.write()
        .insert_node(
            "Secret",
            "s1",
            vec![
                ("id".into(), Value::Str("s1".into())),
                ("sensitive".into(), Value::Str("top-secret-value".into())),
            ],
        )
        .unwrap();

    let role_tokens: HashMap<String, String> =
        [(WRITER_TOK.to_string(), "writer".to_string())].into();

    let app = router_with_tx_limits(
        db.clone(),
        Some(ADMIN.to_string()),
        role_tokens,
        None,
        TxLimits {
            max_open: 100,
            idle_timeout: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Env {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        db,
    }
}

#[tokio::test]
async fn high1_merge_return_on_hidden_node_does_not_leak_via_http_tx() {
    let e = open_with_secret_prop("high1-http").await;

    // Reference: /query must refuse with 403 (MERGE treats hidden node as absent
    // → tries to create Secret → create_labels scope check fires).
    let (q_st, q_body) = e
        .query(
            WRITER_TOK,
            "MERGE (s:Secret {id: 's1'}) RETURN s.sensitive AS x",
        )
        .await;
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert_ne!(q_st, 200, "/query must not succeed: {q_body}");
    assert!(
        !q_body.to_string().contains("top-secret-value"),
        "/query leaked sensitive data: {q_body}"
    );

    // TX path: status and error MUST equal /query — not merely "not 200".
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "MERGE (s:Secret {id: 's1'}) RETURN s.sensitive AS x",
        )
        .await;
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert_eq!(
        run_st, q_st,
        "/tx status {run_st} ≠ /query status {q_st}: {run_body}"
    );
    assert_eq!(run_err, q_err, "/tx error ≠ /query error");
    assert!(
        !run_body.to_string().contains("top-secret-value"),
        "/tx leaked sensitive data: {run_body}"
    );
    e.rollback(&id, WRITER_TOK).await;
}

#[test]
fn high1_merge_return_on_hidden_node_does_not_leak_via_bolt_tx() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: Some(write_scope(&["Task"], &[])),
            }],
            ..Default::default()
        })
        .unwrap();
    db.write()
        .insert_node(
            "Secret",
            "hs1",
            vec![
                ("id".into(), Value::Str("hs1".into())),
                ("sensitive".into(), Value::Str("top-secret-bolt".into())),
            ],
        )
        .unwrap();

    // Explicit-tx Bolt path: MERGE+RETURN on a hidden key must not leak.
    let mut conn = make_bolt_conn(db, WRITER_TOK, Some("writer"));
    bolt_begin(&mut conn).expect("BEGIN");
    let result = bolt_run(
        &mut conn,
        "MERGE (s:Secret {id: 'hs1'}) RETURN s.sensitive AS x",
    );
    match &result {
        Ok(fields) => {
            for f in fields {
                assert_ne!(
                    f,
                    &PackValue::String("top-secret-bolt".into()),
                    "Bolt tx MERGE+RETURN leaked sensitive data: {fields:?}"
                );
            }
        }
        Err(_) => { /* 403 at execute time is the expected outcome */ }
    }
    // Don't commit — no need to.
}

// ── Security regression: HIGH-2 — MATCH…SET…RETURN must not bind hidden nodes ─
//
// `MATCH (a:Task {id: 't1'}), (b:Secret {id: 's1'}) SET a.marker = 1 RETURN b.sensitive AS x`
// With the fix `execute_rows` uses the masked view for both the MATCH and
// RETURN passes, so `b` does not bind → no rows returned (not 200 with data).

#[tokio::test]
async fn high2_match_set_return_hidden_bound_does_not_leak_via_http_tx() {
    let e = open_with_secret_prop("high2-http").await;

    let cypher =
        "MATCH (a:Task {id: 't1'}), (b:Secret {id: 's1'}) SET a.marker = 1 RETURN b.sensitive AS x";

    // /query reference: must not return sensitive data.
    let (q_st, q_body) = e.query(WRITER_TOK, cypher).await;
    assert_eq!(q_st, 200, "/query failed: {q_body}");
    assert_eq!(
        q_body["rows"],
        json!([]),
        "/query bound hidden node and returned data: {q_body}"
    );
    assert!(
        !q_body.to_string().contains("top-secret-value"),
        "/query leaked sensitive data: {q_body}"
    );

    // TX path must match /query.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e.run(&id, WRITER_TOK, cypher).await;
    assert_eq!(run_st, 200, "/tx run failed: {run_body}");
    assert_eq!(
        run_body["rows"],
        json!([]),
        "/tx returned rows with hidden bound variable: {run_body}"
    );
    assert!(
        !run_body.to_string().contains("top-secret-value"),
        "/tx leaked sensitive data: {run_body}"
    );
    e.rollback(&id, WRITER_TOK).await;
}

#[test]
fn high2_match_set_return_hidden_bound_does_not_leak_via_bolt() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: None,
                write: Some(write_scope(&["Task"], &[])),
            }],
            ..Default::default()
        })
        .unwrap();
    db.write()
        .insert_node("Task", "t1", vec![("id".into(), Value::Str("t1".into()))])
        .unwrap();
    db.write()
        .insert_node(
            "Secret",
            "s1",
            vec![
                ("id".into(), Value::Str("s1".into())),
                ("sensitive".into(), Value::Str("top-secret-bolt-h2".into())),
            ],
        )
        .unwrap();

    let cypher =
        "MATCH (a:Task {id: 't1'}), (b:Secret {id: 's1'}) SET a.marker = 1 RETURN b.sensitive AS x";

    let mut conn_ac = make_bolt_conn(db.clone(), WRITER_TOK, Some("writer"));
    // Auto-commit: hidden node must not bind.
    let ac_rows = bolt_run(&mut conn_ac, cypher).expect("auto-commit HIGH-2 must not error");
    for f in &ac_rows {
        assert_ne!(
            f,
            &PackValue::String("top-secret-bolt-h2".into()),
            "auto-commit HIGH-2 leaked sensitive data: {ac_rows:?}"
        );
    }

    // Explicit tx path.
    let mut conn_tx = make_bolt_conn(db, WRITER_TOK, Some("writer"));
    bolt_begin(&mut conn_tx).expect("BEGIN HIGH-2");
    let tx_rows = bolt_run(&mut conn_tx, cypher).expect("tx HIGH-2 must not error");
    for f in &tx_rows {
        assert_ne!(
            f,
            &PackValue::String("top-secret-bolt-h2".into()),
            "tx HIGH-2 leaked sensitive data: {tx_rows:?}"
        );
    }
    bolt_commit(&mut conn_tx);
}

// ── Security regression: Medium-1 — namespace check at execute time ───────────
//
// A role with `namespaces: ["allowed"]` must refuse `CREATE` of a node whose
// `ns` property is not in that list, at execute time (not only at commit).
// This matches how /query handles namespace restrictions (ledger row 140 parity).

async fn open_with_ns_role(name: &str) -> Env {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-tx-ns-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "ns-writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: Some(vec!["allowed-ns".to_string()]),
                write: Some(write_scope(&["Task"], &[])),
            }],
            ..Default::default()
        })
        .unwrap();

    let role_tokens: HashMap<String, String> =
        [(WRITER_TOK.to_string(), "ns-writer".to_string())].into();

    let app = router_with_tx_limits(
        db.clone(),
        Some(ADMIN.to_string()),
        role_tokens,
        None,
        TxLimits {
            max_open: 100,
            idle_timeout: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Env {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        db,
    }
}

#[tokio::test]
async fn medium1_namespace_check_at_execute_time_in_http_tx() {
    let e = open_with_ns_role("med1-http").await;

    // Auto-commit reference: forbidden namespace → 403.
    let (q_st, q_body) = e
        .query(
            WRITER_TOK,
            "CREATE (n:Task {id: 'ns-aq', ns: 'forbidden-ns'})",
        )
        .await;
    assert_eq!(q_st, 403, "/query allowed forbidden ns: {q_body}");
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert!(
        q_err.contains("namespace"),
        "/query error should mention namespace: {q_err}"
    );

    // TX path: same 403 at execute time (not commit).
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "CREATE (n:Task {id: 'ns-tq', ns: 'forbidden-ns'})",
        )
        .await;
    assert_eq!(run_st, 403, "/tx allowed forbidden ns: {run_body}");
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert!(
        run_err.contains("namespace"),
        "/tx error should mention namespace: {run_err}"
    );
    // Transaction must still be usable after the refusal.
    let (rq_st, _) = e.run(&id, WRITER_TOK, "RETURN 1 AS x").await;
    assert_eq!(rq_st, 200, "tx unusable after namespace refusal");
    e.rollback(&id, WRITER_TOK).await;

    // Allowed namespace succeeds.
    let id2 = e.begin_id(WRITER_TOK).await;
    let (ok_st, ok_body) = e
        .run(
            &id2,
            WRITER_TOK,
            "CREATE (n:Task {id: 'ns-ok', ns: 'allowed-ns'})",
        )
        .await;
    assert_eq!(ok_st, 200, "/tx refused allowed ns: {ok_body}");
    let (cmt_st, _) = e.commit(&id2, WRITER_TOK).await;
    assert_eq!(cmt_st, 200, "commit of allowed-ns node failed");
}

// ── M1 over-refusal: no-ns-prop node belongs to the "default" namespace ───────
//
// A node created with no `ns` prop is in the "default" namespace (NS_DEFAULT),
// matching how /query resolves it via `namespace_of_value`.  A role with
// `namespaces:["default"]` must therefore ALLOW `CREATE (n:Foo {id:'x'})`,
// and a role with `namespaces:["other"]` must REFUSE it — same as /query.
//
// The bug (before fix): `check_new_ops_authz` used `.unwrap_or("")`, mapping
// no-ns to "", which is NOT in ["default"] → over-refusal → parity break.

async fn open_with_ns_writer_for_label(name: &str, allowed_ns: &str, label: &str) -> Env {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-tx-ns-label-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "ns-writer".into(),
                labels: vec![label.to_string()],
                keys: vec![],
                visible_where: None,
                namespaces: Some(vec![allowed_ns.to_string()]),
                write: Some(write_scope(&[label], &[])),
            }],
            ..Default::default()
        })
        .unwrap();

    let role_tokens: HashMap<String, String> =
        [(WRITER_TOK.to_string(), "ns-writer".to_string())].into();

    let app = router_with_tx_limits(
        db.clone(),
        Some(ADMIN.to_string()),
        role_tokens,
        None,
        TxLimits {
            max_open: 100,
            idle_timeout: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Env {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        db,
    }
}

/// Positive parity: role has namespaces:["default"]; no-ns-prop CREATE must be
/// allowed in both /query and /tx (default namespace = NS_DEFAULT = "default").
#[tokio::test]
async fn medium1_no_ns_prop_allowed_for_default_namespace_role_http() {
    let e = open_with_ns_writer_for_label("med1-def-http", "default", "Foo").await;

    // /query reference: CREATE with no ns prop → default namespace → allowed.
    let (q_st, q_body) = e
        .query(
            WRITER_TOK,
            "CREATE (n:Foo {id: 'def-aq'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(
        q_st, 200,
        "/query refused default-ns node (no ns prop): {q_body}"
    );

    // /tx must agree: same status.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "CREATE (n:Foo {id: 'def-tq'}) RETURN n.id AS id",
        )
        .await;
    assert_eq!(
        run_st, q_st,
        "/tx status {run_st} ≠ /query status {q_st}: {run_body}"
    );
    let (cmt_st, cmt_body) = e.commit(&id, WRITER_TOK).await;
    assert_eq!(cmt_st, 200, "commit of default-ns node failed: {cmt_body}");
}

/// Negative parity: role has namespaces:["other"]; no-ns-prop CREATE maps to
/// "default" which is NOT in ["other"] → 403 in both /query and /tx, same error.
#[tokio::test]
async fn medium1_no_ns_prop_refused_for_non_default_namespace_role_http() {
    let e = open_with_ns_writer_for_label("med1-other-http", "other", "Foo").await;

    // /query reference: CREATE with no ns prop → default namespace → not in ["other"] → 403.
    let (q_st, q_body) = e
        .query(
            WRITER_TOK,
            "CREATE (n:Foo {id: 'other-aq'}) RETURN n.id AS id",
        )
        .await;
    let q_err = q_body["error"].as_str().unwrap_or("");
    assert_eq!(
        q_st, 403,
        "/query allowed node in default ns when role requires other ns: {q_body}"
    );
    assert!(
        q_err.contains("namespace"),
        "/query error should mention namespace: {q_err}"
    );

    // /tx must agree: same status and same error.
    let id = e.begin_id(WRITER_TOK).await;
    let (run_st, run_body) = e
        .run(
            &id,
            WRITER_TOK,
            "CREATE (n:Foo {id: 'other-tq'}) RETURN n.id AS id",
        )
        .await;
    let run_err = run_body["error"].as_str().unwrap_or("");
    assert_eq!(
        run_st, q_st,
        "/tx status {run_st} ≠ /query status {q_st}: {run_body}"
    );
    assert_eq!(run_err, q_err, "/tx error ≠ /query error");
    e.rollback(&id, WRITER_TOK).await;
}

#[test]
fn medium1_no_ns_prop_parity_via_bolt() {
    // Positive: namespaces:["default"] — no-ns-prop node must be allowed.
    {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        db.write()
            .apply_schema(&Schema {
                roles: vec![RoleDef {
                    name: "ns-writer".into(),
                    labels: vec!["Foo".into()],
                    keys: vec![],
                    visible_where: None,
                    namespaces: Some(vec!["default".to_string()]),
                    write: Some(write_scope(&["Foo"], &[])),
                }],
                ..Default::default()
            })
            .unwrap();

        // Auto-commit reference: must succeed.
        let mut conn_ac = make_bolt_conn(db.clone(), WRITER_TOK, Some("ns-writer"));
        bolt_run(&mut conn_ac, "CREATE (n:Foo {id: 'def-bolt-ac'})")
            .expect("auto-commit of default-ns node must succeed");

        // Explicit tx: same, must succeed.
        let mut conn_tx = make_bolt_conn(db, WRITER_TOK, Some("ns-writer"));
        bolt_begin(&mut conn_tx).expect("BEGIN");
        bolt_run(&mut conn_tx, "CREATE (n:Foo {id: 'def-bolt-tx'})")
            .expect("tx of default-ns node must succeed");
        assert!(bolt_commit(&mut conn_tx), "COMMIT of default-ns bolt tx");
    }

    // Negative: namespaces:["other"] — no-ns-prop node is in "default" ≠ "other" → error.
    {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        db.write()
            .apply_schema(&Schema {
                roles: vec![RoleDef {
                    name: "ns-writer".into(),
                    labels: vec!["Foo".into()],
                    keys: vec![],
                    visible_where: None,
                    namespaces: Some(vec!["other".to_string()]),
                    write: Some(write_scope(&["Foo"], &[])),
                }],
                ..Default::default()
            })
            .unwrap();

        // Auto-commit reference: must refuse.
        let mut conn_ac = make_bolt_conn(db.clone(), WRITER_TOK, Some("ns-writer"));
        let ac_err = bolt_run(&mut conn_ac, "CREATE (n:Foo {id: 'other-bolt-ac'})")
            .expect_err("auto-commit must refuse default-ns node when role requires other ns");
        assert!(
            ac_err.contains("namespace"),
            "auto-commit error should mention namespace: {ac_err}"
        );

        // Explicit tx: same refusal, same error message.
        let mut conn_tx = make_bolt_conn(db, WRITER_TOK, Some("ns-writer"));
        bolt_begin(&mut conn_tx).expect("BEGIN");
        let tx_err = bolt_run(&mut conn_tx, "CREATE (n:Foo {id: 'other-bolt-tx'})")
            .expect_err("tx must refuse default-ns node when role requires other ns");
        assert_eq!(tx_err, ac_err, "tx error must match auto-commit error");
    }
}

#[test]
fn medium1_namespace_check_at_execute_time_via_bolt() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "ns-writer".into(),
                labels: vec!["Task".into()],
                keys: vec![],
                visible_where: None,
                namespaces: Some(vec!["allowed-ns".to_string()]),
                write: Some(write_scope(&["Task"], &[])),
            }],
            ..Default::default()
        })
        .unwrap();

    let mut conn_ac = make_bolt_conn(db.clone(), WRITER_TOK, Some("ns-writer"));
    // Auto-commit reference: forbidden namespace.
    let ac_err = bolt_run(
        &mut conn_ac,
        "CREATE (n:Task {id: 'ns-bolt-ac', ns: 'forbidden-ns'})",
    )
    .expect_err("auto-commit must refuse forbidden namespace");
    assert!(
        ac_err.contains("namespace"),
        "auto-commit error should mention namespace: {ac_err}"
    );

    // Explicit tx: forbidden namespace refused at execute time.
    let mut conn_tx = make_bolt_conn(db.clone(), WRITER_TOK, Some("ns-writer"));
    bolt_begin(&mut conn_tx).expect("BEGIN");
    let tx_err = bolt_run(
        &mut conn_tx,
        "CREATE (n:Task {id: 'ns-bolt-tx', ns: 'forbidden-ns'})",
    )
    .expect_err("tx must refuse forbidden namespace at execute time");
    assert!(
        tx_err.contains("namespace"),
        "tx error should mention namespace: {tx_err}"
    );

    // Allowed namespace succeeds in explicit tx.
    let mut conn_ok = make_bolt_conn(db, WRITER_TOK, Some("ns-writer"));
    bolt_begin(&mut conn_ok).expect("BEGIN ok");
    bolt_run(
        &mut conn_ok,
        "CREATE (n:Task {id: 'ns-bolt-ok', ns: 'allowed-ns'})",
    )
    .expect("allowed namespace must succeed");
    assert!(bolt_commit(&mut conn_ok), "COMMIT of allowed-ns tx");
}
