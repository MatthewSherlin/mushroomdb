//! HTTP / Bolt parity for scoped identities (TX-17).
//!
//! A Bolt connection authenticated as a role token, or with a `read` key, must
//! see and change exactly what the same credential sees and changes over HTTP:
//! the same rows, the same refusals, the same kind of error. This file runs one
//! script of reads and writes through `POST /query` and through Bolt
//! `RUN`/`PULL`, and one through `/tx/*` and Bolt `BEGIN`/`RUN`/`COMMIT`, for
//! each identity, each protocol on its own copy of one fixture, and compares
//! every answer and the stores they leave behind.
//!
//! The fixture has two namespaces and two roles bound to one each, so both
//! visibility axes (label and namespace) and both write paths (a role with a
//! write scope, a role without) are covered.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use axum::Router;
use bolt::connection::Connection;
use bolt::error::{BoltError, Classification};
use bolt::messages::hello::{AuthToken, Hello, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::PackValue;
use mushroomdb::{AccessKeys, BatchOp, RoleDef, Schema, SharedDb, Value, WriteScope};
use serde_json::{json, Value as Json};
use tower::ServiceExt;

const FULL: &str = "parity-full-token";
const ALPHA: &str = "parity-alpha-token";
const BETA: &str = "parity-beta-token";
const RD: &str = "rd-key-0123456789abcdef0123456789abcdef";
const WR: &str = "wr-key-0123456789abcdef0123456789abcdef";
/// A role token whose role the store does not define: the state a token is in
/// once its role is removed. Roles are looked up per statement on both
/// protocols, never cached at login, so this is the same path as a role
/// deleted after `HELLO` (the store has no public call that removes a role).
const GHOST: &str = "parity-ghost-token";

/// Every credential the comparison runs as. The full token and the `write` key
/// are the controls: an unscoped identity must agree too.
const IDENTITIES: [(&str, &str); 5] = [
    ("full token", FULL),
    ("write key", WR),
    ("alpha role token", ALPHA),
    ("beta role token", BETA),
    ("read key bound to alpha", RD),
];

fn role_tokens() -> HashMap<String, String> {
    [(ALPHA, "alpha"), (BETA, "beta"), (GHOST, "ghost")]
        .into_iter()
        .map(|(t, r)| (t.to_string(), r.to_string()))
        .collect()
}

fn keys() -> AccessKeys {
    let text = format!(
        r#"{{"keys":[{{"hash":"{}","tier":"read","role":"alpha"}},{{"hash":"{}","tier":"write"}}]}}"#,
        mushroomdb::hash_token(RD).unwrap(),
        mushroomdb::hash_token(WR).unwrap()
    );
    AccessKeys::from_json(&text).unwrap()
}

fn node(label: &str, key: &str, ns: &str) -> BatchOp {
    BatchOp::InsertNode {
        label: label.into(),
        key: key.into(),
        props: vec![
            ("id".into(), Value::Str(key.into())),
            ("ns".into(), Value::Str(ns.into())),
        ],
    }
}

fn edge(src: &str, dst: &str) -> BatchOp {
    BatchOp::InsertEdge {
        edge_type: "REFS".into(),
        src_key: src.into(),
        dst_key: dst.into(),
    }
}

/// `alpha` sees `Doc` in `tenant-a` and may create, update and delete `Doc`
/// and create `REFS`; `beta` sees `Doc` and `Note` in `tenant-b` and writes
/// nothing. `Secret` is visible to neither role.
fn fixture() -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            roles: vec![
                RoleDef {
                    name: "alpha".into(),
                    keys: vec![],
                    labels: vec!["Doc".into()],
                    visible_where: None,
                    namespaces: Some(vec!["tenant-a".into()]),
                    write: Some(WriteScope {
                        create_labels: vec!["Doc".into()],
                        update_labels: vec!["Doc".into()],
                        delete_labels: vec!["Doc".into()],
                        create_edge_types: vec!["REFS".into()],
                        update_edge_types: vec![],
                        delete_edge_types: vec![],
                    }),
                },
                RoleDef {
                    name: "beta".into(),
                    keys: vec![],
                    labels: vec!["Doc".into(), "Note".into()],
                    visible_where: None,
                    namespaces: Some(vec!["tenant-b".into()]),
                    write: None,
                },
            ],
            ..Default::default()
        })
        .unwrap();
    db.submit_batch(vec![
        node("Doc", "a1", "tenant-a"),
        node("Doc", "a2", "tenant-a"),
        node("Secret", "s1", "tenant-a"),
        node("Doc", "b1", "tenant-b"),
        node("Note", "n1", "tenant-b"),
        edge("a1", "a2"),
        edge("a1", "s1"),
        edge("b1", "n1"),
    ])
    .unwrap();
    for ddl in [
        "CREATE INDEX doc_id FOR (n:Doc) ON (n.id)",
        "CREATE INDEX note_id FOR (n:Note) ON (n.id)",
        "CREATE INDEX secret_id FOR (n:Secret) ON (n.id)",
        "CREATE CONSTRAINT secret_unique FOR (n:Secret) REQUIRE n.id IS UNIQUE",
    ] {
        db.write().query_write(ddl, &BTreeMap::new()).unwrap();
    }
    (db, dir)
}

/// A statement and its one optional string parameter `$id`.
type Statement = (&'static str, Option<&'static str>);

const READS: &[Statement] = &[
    ("MATCH (n) RETURN n.id ORDER BY n.id", None),
    ("MATCH (n) RETURN n", None),
    ("MATCH (n:Doc) RETURN n.id, n.ns ORDER BY n.id", None),
    ("MATCH (n) RETURN COUNT(n)", None),
    (
        "MATCH (a)-[:REFS]->(b) RETURN a.id, b.id ORDER BY a.id",
        None,
    ),
    ("MATCH (n:Secret) RETURN n.id", None),
    ("MATCH (n {id: $id}) RETURN n.id", Some("b1")),
    ("MATCH (n {id: $id}) RETURN n.id", Some("a1")),
    ("SHOW INDEXES", None),
    ("SHOW CONSTRAINTS", None),
    ("RETURN 1", None),
    ("MATCH (n RETURN n", None),
];

const WRITES: &[Statement] = &[
    ("CREATE (n:Doc {id: 'c1', ns: 'tenant-a'})", None),
    ("CREATE (n:Doc {id: 'c2', ns: 'tenant-b'})", None),
    ("CREATE (n:Secret {id: 'c3', ns: 'tenant-a'})", None),
    ("CREATE (n:Note {id: 'c4', ns: 'tenant-b'})", None),
    ("MATCH (n:Doc {id: 'a1'}) SET n.v = 1", None),
    ("MATCH (n:Doc {id: 'b1'}) SET n.v = 2", None),
    ("MATCH (n:Secret {id: 's1'}) SET n.v = 3", None),
    ("MATCH (n:Doc {id: $id}) SET n.w = 4", Some("a2")),
    ("MERGE (n:Doc {id: 'm1', ns: 'tenant-a'})", None),
    (
        "UNWIND [1] AS x MATCH (a:Doc {id: 'a1'}), (b:Doc {id: 'c1'}) CREATE (a)-[:REFS]->(b)",
        None,
    ),
    ("MATCH (n:Doc {id: 'a2'}) DETACH DELETE n", None),
    ("MATCH (n:Doc {id: 'b1'}) DETACH DELETE n", None),
    ("CREATE INDEX v_idx FOR (n:Doc) ON (n.v)", None),
    ("DROP INDEX doc_id", None),
    (
        "CREATE CONSTRAINT doc_unique FOR (n:Doc) REQUIRE n.id IS UNIQUE",
        None,
    ),
    ("DROP CONSTRAINT secret_unique", None),
];

/// What a request came to, in terms both protocols share.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Rows { columns: Vec<String>, rows: Json },
    Refused { kind: Kind, message: String },
}

/// The kind of a refusal: HTTP's status class and Bolt's code class, mapped
/// onto one another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// HTTP 4xx other than 401/403; Bolt `ClientError` other than security.
    Client,
    Unauthorized,
    Forbidden,
    /// HTTP 500; Bolt `DatabaseError`.
    Database,
    /// HTTP 503; Bolt `TransientError`.
    Transient,
}

fn http_kind(status: u16) -> Kind {
    match status {
        401 => Kind::Unauthorized,
        403 => Kind::Forbidden,
        400..=499 => Kind::Client,
        503 => Kind::Transient,
        _ => Kind::Database,
    }
}

fn bolt_kind(err: &BoltError) -> Kind {
    match (err, err.classification()) {
        (BoltError::Unauthorized, _) => Kind::Unauthorized,
        (BoltError::Forbidden(_), _) => Kind::Forbidden,
        (_, Classification::Client) => Kind::Client,
        (_, Classification::Transient) => Kind::Transient,
        (_, Classification::Database) => Kind::Database,
    }
}

/// The same answer, or the same refusal: equal kinds, and for a `403` the
/// same text. A client error's Bolt text carries the HTTP text.
fn assert_same(label: &str, http: &Outcome, bolt: &Outcome) {
    match (http, bolt) {
        (Outcome::Rows { .. }, Outcome::Rows { .. }) => {
            assert_eq!(http, bolt, "{label}: rows differ");
        }
        (
            Outcome::Refused {
                kind: hk,
                message: hm,
            },
            Outcome::Refused {
                kind: bk,
                message: bm,
            },
        ) => {
            assert_eq!(hk, bk, "{label}: HTTP {hm:?}, Bolt {bm:?}");
            match hk {
                Kind::Forbidden => assert_eq!(hm, bm, "{label}: refusal text differs"),
                Kind::Client => assert!(
                    !hm.is_empty() && bm.contains(hm.as_str()),
                    "{label}: Bolt {bm:?} does not carry HTTP {hm:?}"
                ),
                _ => {}
            }
        }
        _ => panic!("{label}: HTTP {http:?}, Bolt {bolt:?}"),
    }
}

// ── HTTP side ───────────────────────────────────────────────────────────────

struct Http {
    app: Router,
    token: &'static str,
}

impl Http {
    fn new(db: &SharedDb, token: &'static str) -> Self {
        let app =
            server::router_with_access_keys(db.clone(), Some(FULL.into()), role_tokens(), keys());
        Self { app, token }
    }

    async fn post(&self, path: &str, body: Json) -> (u16, Json) {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Json::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, body)
    }

    fn outcome(status: u16, body: Json) -> Outcome {
        if status == 200 {
            let columns = body["columns"]
                .as_array()
                .unwrap_or_else(|| panic!("no columns: {body}"))
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect();
            Outcome::Rows {
                columns,
                rows: body["rows"].clone(),
            }
        } else {
            Outcome::Refused {
                kind: http_kind(status),
                message: body["error"].as_str().unwrap_or_default().to_string(),
            }
        }
    }

    fn body((cypher, id): Statement) -> Json {
        match id {
            Some(id) => json!({"cypher": cypher, "params": {"id": id}}),
            None => json!({"cypher": cypher}),
        }
    }

    async fn query(&self, statement: Statement) -> Outcome {
        let (status, body) = self.post("/query?format=json", Self::body(statement)).await;
        Self::outcome(status, body)
    }

    /// `Ok(tx_id)`, or the refusal.
    async fn begin(&self) -> Result<String, Outcome> {
        let (status, body) = self.post("/tx/begin", json!({})).await;
        if status == 200 {
            Ok(body["tx_id"].as_str().unwrap().to_string())
        } else {
            Err(Self::outcome(status, body))
        }
    }

    async fn run(&self, tx: &str, statement: Statement) -> Outcome {
        let (status, body) = self
            .post(&format!("/tx/{tx}/run"), Self::body(statement))
            .await;
        Self::outcome(status, body)
    }

    /// Whether the commit succeeded.
    async fn commit(&self, tx: &str) -> bool {
        self.post(&format!("/tx/{tx}/commit"), json!({})).await.0 == 200
    }

    async fn rollback(&self, tx: &str) {
        let _ = self.post(&format!("/tx/{tx}/rollback"), json!({})).await;
    }
}

// ── Bolt side ───────────────────────────────────────────────────────────────

/// A Bolt connection on the same credentials the HTTP router has, as
/// `mushroomdb serve --bolt-port` builds it.
fn bolt_connection(db: &SharedDb, token: &str) -> Connection {
    let auth = TokenAuthenticator::new(Some(FULL.into()), role_tokens()).with_access_keys(keys());
    let mut conn = Connection::new(Arc::new(auth), Duration::from_secs(30), "p".into(), (5, 0))
        .with_database(db.clone(), Duration::from_millis(100));
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: token.into(),
        },
    }));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn
}

fn pack_to_json(value: &PackValue) -> Json {
    match value {
        PackValue::Null => Json::Null,
        PackValue::Bool(b) => json!(b),
        PackValue::Int(i) => json!(i),
        PackValue::Float(f) => json!(f),
        PackValue::String(s) => json!(s),
        PackValue::List(items) => Json::Array(items.iter().map(pack_to_json).collect()),
        PackValue::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), pack_to_json(v)))
                .collect(),
        ),
        // A node (TX-NS) is compared by the key HTTP renders it as: its
        // element id is `n:<key>`.
        PackValue::Struct(node) if node.tag == 0x4E => match node.fields.get(3) {
            Some(PackValue::String(element_id)) => json!(element_id.strip_prefix("n:")),
            other => panic!("a Bolt 5 node carries its element id: {other:?}"),
        },
        other => panic!("the parity fixture holds no {other:?}"),
    }
}

fn bolt_run_message((cypher, id): Statement) -> ClientMessage {
    let params = match id {
        Some(id) => vec![("id".to_string(), PackValue::String(id.into()))],
        None => vec![],
    };
    ClientMessage::Other(
        MessageKind::Run,
        vec![
            PackValue::String(cypher.into()),
            PackValue::Map(params),
            PackValue::Map(vec![]),
        ],
    )
}

fn bolt_refused(err: &BoltError) -> Outcome {
    Outcome::Refused {
        kind: bolt_kind(err),
        message: err.message(),
    }
}

/// `RUN` then `PULL` everything. A failure is answered with `RESET` in
/// auto-commit, so the next statement starts clean; in a transaction the
/// caller decides.
fn bolt_statement(conn: &mut Connection, statement: Statement, in_tx: bool) -> Outcome {
    let out = conn.handle(bolt_run_message(statement));
    let columns = match out.as_slice() {
        [ServerMessage::Success(meta)] => match meta.iter().find(|(k, _)| k == "fields") {
            Some((_, PackValue::List(fields))) => fields
                .iter()
                .map(|f| match f {
                    PackValue::String(s) => s.clone(),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>(),
            other => panic!("no fields: {other:?}"),
        },
        [ServerMessage::Failure(err)] => {
            let outcome = bolt_refused(err);
            if !in_tx {
                conn.handle(ClientMessage::Reset);
            }
            return outcome;
        }
        other => panic!("RUN answered {other:?}"),
    };
    let out = conn.handle(ClientMessage::Other(
        MessageKind::Pull,
        vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
    ));
    let mut rows = Vec::new();
    for message in &out {
        match message {
            ServerMessage::Record(row) => {
                rows.push(Json::Array(row.iter().map(pack_to_json).collect()))
            }
            ServerMessage::Success(_) => {}
            ServerMessage::Failure(err) => return bolt_refused(err),
            other => panic!("PULL answered {other:?}"),
        }
    }
    Outcome::Rows {
        columns,
        rows: Json::Array(rows),
    }
}

/// `Ok(())`, or the refusal `BEGIN` got.
fn bolt_begin(conn: &mut Connection) -> Result<(), Outcome> {
    let begin = ClientMessage::Other(MessageKind::Begin, vec![PackValue::Map(vec![])]);
    match conn.handle(begin).as_slice() {
        [ServerMessage::Success(_)] => Ok(()),
        [ServerMessage::Failure(err)] => {
            let outcome = bolt_refused(err);
            conn.handle(ClientMessage::Reset);
            Err(outcome)
        }
        other => panic!("BEGIN answered {other:?}"),
    }
}

fn bolt_commit(conn: &mut Connection) -> bool {
    let out = conn.handle(ClientMessage::Other(MessageKind::Commit, vec![]));
    matches!(out.as_slice(), [ServerMessage::Success(_)])
}

// ── the comparison ──────────────────────────────────────────────────────────

/// Everything in a store, read with no scope: what a script left behind.
fn dump(db: &SharedDb) -> Json {
    let params = BTreeMap::new();
    let reader = db.reader();
    let mut out = Vec::new();
    for q in [
        "MATCH (n) RETURN n ORDER BY n.id",
        "MATCH (a)-[:REFS]->(b) RETURN a.id, b.id ORDER BY a.id",
        "SHOW INDEXES",
        "SHOW CONSTRAINTS",
    ] {
        let rs = reader.query(q, &params).unwrap();
        let rows: Vec<Json> = (0..rs.len())
            .map(|i| {
                Json::Array(
                    rs.row(i)
                        .iter()
                        .map(|cell| match cell {
                            Some(v) => json!(format!("{v:?}")),
                            None => Json::Null,
                        })
                        .collect(),
                )
            })
            .collect();
        out.push(json!({ "query": q, "rows": rows }));
    }
    Json::Array(out)
}

/// The answers a script got, by shape, so a test can show it reached rows,
/// refusals and request errors and did not agree only on one of them.
fn shape(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Rows { .. } => "rows",
        Outcome::Refused {
            kind: Kind::Forbidden,
            ..
        } => "forbidden",
        Outcome::Refused {
            kind: Kind::Client, ..
        } => "client",
        Outcome::Refused { .. } => "other",
    }
}

#[tokio::test]
async fn auto_commit_reads_and_writes_agree_for_every_identity() {
    let mut seen = std::collections::BTreeSet::new();
    for (who, token) in IDENTITIES {
        let (http_db, _h) = fixture();
        let (bolt_db, _b) = fixture();
        let http = Http::new(&http_db, token);
        let mut bolt = bolt_connection(&bolt_db, token);
        // Reads, then the writes, then the reads again over what they left.
        let script = READS.iter().chain(WRITES).chain(READS);
        for (step, statement) in script.enumerate() {
            let label = format!("{who}, step {step}: {}", statement.0);
            let h = http.query(*statement).await;
            let b = bolt_statement(&mut bolt, *statement, false);
            assert_same(&label, &h, &b);
            seen.insert((token, shape(&b)));
        }
        assert_eq!(
            dump(&http_db),
            dump(&bolt_db),
            "{who}: the stores the two protocols left differ"
        );
    }
    // Each role both wrote and was refused; every identity hit a request error.
    for token in [FULL, WR, ALPHA, BETA, RD] {
        assert!(seen.contains(&(token, "rows")), "{token}: {seen:?}");
        assert!(seen.contains(&(token, "client")), "{token}: {seen:?}");
    }
    for token in [ALPHA, BETA, RD] {
        assert!(seen.contains(&(token, "forbidden")), "{token}: {seen:?}");
    }
}

/// Explicit transactions. A role token and a `read` key read through the role
/// on the snapshot their transaction began on and refuse every write with the
/// text HTTP gives (ledger row 140: such a transaction does not write, even
/// for a role with a write scope). The full token runs the transaction engine
/// on both protocols.
#[tokio::test]
async fn transactions_agree_for_every_identity() {
    let mut seen = std::collections::BTreeSet::new();
    for (who, token) in IDENTITIES {
        let (http_db, _h) = fixture();
        let (bolt_db, _b) = fixture();
        let http = Http::new(&http_db, token);
        let mut bolt = bolt_connection(&bolt_db, token);

        // One transaction of reads, with a commit landing after it began.
        let h_tx = http.begin().await;
        let b_tx = bolt_begin(&mut bolt);
        assert_eq!(h_tx.is_ok(), b_tx.is_ok(), "{who}: BEGIN {h_tx:?} {b_tx:?}");
        let Ok(h_tx) = h_tx else {
            assert_same(
                &format!("{who}: BEGIN"),
                &h_tx.unwrap_err(),
                &b_tx.unwrap_err(),
            );
            continue;
        };
        for db in [&http_db, &bolt_db] {
            db.submit_batch(vec![node("Doc", "late", "tenant-a")])
                .unwrap();
        }
        // The last read is the syntax error; it gets a transaction of its own
        // below, so this one reaches COMMIT.
        let (bad, good) = READS.split_last().unwrap();
        for (step, statement) in good.iter().enumerate() {
            let label = format!("{who}, tx read {step}: {}", statement.0);
            let h = http.run(&h_tx, *statement).await;
            let b = bolt_statement(&mut bolt, *statement, true);
            assert_same(&label, &h, &b);
            seen.insert((token, shape(&b)));
        }
        assert_eq!(bolt.state().name(), "InTx", "{who}");
        let (h_commit, b_commit) = (http.commit(&h_tx).await, bolt_commit(&mut bolt));
        assert!(h_commit && b_commit, "{who}: COMMIT {h_commit} {b_commit}");

        // Each write, and the syntax error, in a transaction of its own,
        // committed if it ran.
        for (step, statement) in WRITES.iter().chain([bad]).enumerate() {
            let label = format!("{who}, tx write {step}: {}", statement.0);
            let h_tx = http.begin().await.expect("BEGIN worked above");
            bolt_begin(&mut bolt).expect("BEGIN worked above");
            let h = http.run(&h_tx, *statement).await;
            let b = bolt_statement(&mut bolt, *statement, true);
            assert_same(&label, &h, &b);
            seen.insert((token, shape(&b)));
            if matches!(b, Outcome::Rows { .. }) {
                assert_eq!(
                    http.commit(&h_tx).await,
                    bolt_commit(&mut bolt),
                    "{label}: COMMIT"
                );
            } else {
                http.rollback(&h_tx).await;
                bolt.handle(ClientMessage::Reset);
            }
        }
        assert_eq!(
            dump(&http_db),
            dump(&bolt_db),
            "{who}: the stores the two protocols left differ"
        );
    }
    for token in [FULL, WR, ALPHA, BETA, RD] {
        assert!(seen.contains(&(token, "rows")), "{token}: {seen:?}");
        assert!(seen.contains(&(token, "client")), "{token}: {seen:?}");
    }
    for token in [ALPHA, BETA, RD] {
        assert!(seen.contains(&(token, "forbidden")), "{token}: {seen:?}");
    }
}

/// A token whose role is gone logs in on both protocols (neither checks the
/// role at login) and is then unauthorized for a read, a write and `BEGIN`:
/// HTTP 401, Bolt `Unauthorized`. Nothing is written.
#[tokio::test]
async fn a_vanished_role_is_unauthorized_on_both() {
    let (http_db, _h) = fixture();
    let (bolt_db, _b) = fixture();
    let before = dump(&http_db);
    let http = Http::new(&http_db, GHOST);
    let mut bolt = bolt_connection(&bolt_db, GHOST);
    let unauthorized = |o: &Outcome| {
        matches!(
            o,
            Outcome::Refused {
                kind: Kind::Unauthorized,
                ..
            }
        )
    };
    for statement in [READS[0], WRITES[0], WRITES[4]] {
        let label = format!("ghost: {}", statement.0);
        let h = http.query(statement).await;
        let b = bolt_statement(&mut bolt, statement, false);
        assert_same(&label, &h, &b);
        assert!(unauthorized(&h), "{label}: {h:?}");
    }
    let h = http.begin().await.expect_err("BEGIN for a vanished role");
    let b = bolt_begin(&mut bolt).expect_err("BEGIN for a vanished role");
    assert_same("ghost: BEGIN", &h, &b);
    assert!(unauthorized(&h), "{h:?}");
    assert_eq!(dump(&http_db), before);
    assert_eq!(dump(&bolt_db), before);
}

/// A credential neither side knows is refused by both, before anything runs.
#[tokio::test]
async fn an_unknown_credential_is_refused_by_both() {
    let (db, _dir) = fixture();
    let http = Http::new(&db, "not-a-token");
    let h = http.query(READS[0]).await;
    assert_eq!(
        h,
        Outcome::Refused {
            kind: Kind::Unauthorized,
            message: "unauthorized".into()
        }
    );
    let auth = TokenAuthenticator::new(Some(FULL.into()), role_tokens()).with_access_keys(keys());
    let mut conn = Connection::new(Arc::new(auth), Duration::from_secs(30), "p".into(), (5, 0))
        .with_database(db.clone(), Duration::from_millis(100));
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: "not-a-token".into(),
        },
    }));
    assert!(
        matches!(
            out.as_slice(),
            [ServerMessage::Failure(BoltError::Unauthorized)]
        ),
        "{out:?}"
    );
}

/// QL-NM review C1: a MERGE pattern that names a hidden node's `id` beside
/// other properties answers, over HTTP and Bolt alike, exactly as it does
/// when the key does not exist, or as the key MERGE of that key does. It
/// never binds the hidden node or says that it exists.
#[tokio::test]
async fn a_pattern_merge_never_reveals_a_hidden_node() {
    // (probe on the hidden `s1`, the same probe on an absent key or the key
    // MERGE it must answer like)
    let pairs: [(Statement, Statement); 4] = [
        (
            ("MERGE (n {id: 's1', name: 'x'}) RETURN n.name", None),
            ("MERGE (n {id: 'zz', name: 'x'}) RETURN n.name", None),
        ),
        (
            ("MERGE (n:Secret {id: 's1', name: 'x'})", None),
            ("MERGE (n:Secret {id: 'zz', name: 'x'})", None),
        ),
        (
            ("MERGE (n {name: 'x'})", None),
            ("MERGE (n {name: 'x'})", None),
        ),
        (
            ("MERGE (n:Doc {id: 's1', name: 'zz'})", None),
            ("MERGE (n:Doc {id: 's1'})", None),
        ),
    ];
    for (probe, reference) in pairs {
        let mut answers = Vec::new();
        for statement in [probe, reference] {
            let (http_db, _h) = fixture();
            let (bolt_db, _b) = fixture();
            let h = Http::new(&http_db, ALPHA).query(statement).await;
            let b = bolt_statement(&mut bolt_connection(&bolt_db, ALPHA), statement, false);
            assert_same(statement.0, &h, &b);
            if let Outcome::Refused { message, .. } = &h {
                assert!(!message.contains("exists"), "{}: {message}", statement.0);
            }
            answers.push(h);
        }
        assert_eq!(answers[0], answers[1], "{} vs {}", probe.0, reference.0);
        assert!(
            matches!(
                answers[0],
                Outcome::Refused {
                    kind: Kind::Forbidden,
                    ..
                }
            ),
            "{}: {:?}",
            probe.0,
            answers[0]
        );
    }
}
