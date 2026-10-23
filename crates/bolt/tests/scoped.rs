//! Scoped visibility per Bolt connection (TX-17): a connection authenticated
//! as a role, or with a `read` key, sees and changes exactly what that identity
//! sees and changes over HTTP `/query` and `/tx`. The HTTP side of the
//! comparison is `crates/server/tests/bolt_parity.rs`; these tests pin the Bolt
//! behaviour on its own.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bolt::connection::Connection;
use bolt::error::BoltError;
use bolt::messages::hello::{AuthToken, Hello, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::PackValue;
use mushroomdb::{AccessKeys, BatchOp, RoleDef, Schema, SharedDb, Value, WriteScope};

const FULL: &str = "full-secret";
const ALPHA: &str = "alpha-secret";
const BETA: &str = "beta-secret";
const RD: &str = "rd-key-0123456789abcdef0123456789abcdef";

/// The refusal HTTP `/tx/{id}/run` gives a role token's write.
const TX_ROLE_WRITE_REFUSAL: &str =
    "role-bound token: writes are not permitted inside a transaction";
/// The refusal HTTP gives a `read` key's write, in `/query` and in `/tx`.
const READ_TIER_REFUSAL: &str = "read-tier key: writes are not permitted";

fn keys() -> AccessKeys {
    let text = format!(
        r#"{{"keys":[{{"hash":"{}","tier":"read","role":"alpha"}}]}}"#,
        mushroomdb::hash_token(RD).unwrap()
    );
    AccessKeys::from_json(&text).unwrap()
}

fn authenticator() -> Arc<TokenAuthenticator> {
    let mut roles = HashMap::new();
    roles.insert(ALPHA.to_string(), "alpha".to_string());
    roles.insert(BETA.to_string(), "beta".to_string());
    Arc::new(TokenAuthenticator::new(Some(FULL.into()), roles).with_access_keys(keys()))
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

/// Two namespaces, two roles bound to one each. `alpha` sees `Doc` in
/// `tenant-a` and may write `Doc`; `beta` sees `Doc` and `Note` in `tenant-b`
/// and writes nothing. `Secret` is visible to neither.
fn store() -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let doc_scope = WriteScope {
        create_labels: vec!["Doc".into()],
        update_labels: vec!["Doc".into()],
        delete_labels: vec!["Doc".into()],
        ..WriteScope::default()
    };
    db.write()
        .apply_schema(&Schema {
            roles: vec![
                RoleDef {
                    name: "alpha".into(),
                    keys: vec![],
                    labels: vec!["Doc".into()],
                    visible_where: None,
                    namespaces: Some(vec!["tenant-a".into()]),
                    write: Some(doc_scope),
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
    ])
    .unwrap();
    for ddl in [
        "CREATE INDEX doc_id FOR (n:Doc) ON (n.id)",
        "CREATE INDEX note_id FOR (n:Note) ON (n.id)",
        "CREATE INDEX secret_id FOR (n:Secret) ON (n.id)",
    ] {
        db.write().query_write(ddl, &BTreeMap::new()).unwrap();
    }
    (db, dir)
}

fn connected(db: &SharedDb, token: &str) -> Connection {
    let mut conn = Connection::new(authenticator(), Duration::from_secs(30), "c".into(), (5, 0))
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

fn run(query: &str) -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Run,
        vec![
            PackValue::String(query.into()),
            PackValue::Map(vec![]),
            PackValue::Map(vec![]),
        ],
    )
}

fn pull_all() -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Pull,
        vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
    )
}

fn begin() -> ClientMessage {
    ClientMessage::Other(MessageKind::Begin, vec![PackValue::Map(vec![])])
}

fn commit() -> ClientMessage {
    ClientMessage::Other(MessageKind::Commit, vec![])
}

/// Runs `query` and pulls every row; panics on a `FAILURE`.
fn rows(conn: &mut Connection, query: &str) -> Vec<Vec<PackValue>> {
    let out = conn.handle(run(query));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{query}: {out:?}"
    );
    let out = conn.handle(pull_all());
    let mut rows = Vec::new();
    for message in out {
        match message {
            ServerMessage::Record(row) => rows.push(row),
            ServerMessage::Success(_) => {}
            other => panic!("{query}: {other:?}"),
        }
    }
    rows
}

/// The first column of every row, as strings.
fn column(conn: &mut Connection, query: &str) -> Vec<String> {
    rows(conn, query)
        .into_iter()
        .map(|row| match row.first() {
            Some(PackValue::String(s)) => s.clone(),
            other => panic!("{query}: not a string: {other:?}"),
        })
        .collect()
}

/// The error of the `FAILURE` `message` is answered with.
fn failure(conn: &mut Connection, message: ClientMessage) -> BoltError {
    match conn.handle(message).as_slice() {
        [ServerMessage::Failure(err)] => err.clone(),
        other => panic!("expected one FAILURE, got {other:?}"),
    }
}

fn ids(db: &SharedDb) -> Vec<String> {
    let rs = db
        .reader()
        .query("MATCH (n) RETURN n.id ORDER BY n.id", &BTreeMap::new())
        .unwrap();
    (0..rs.len())
        .map(|i| match rs.get(i, "n.id") {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

const ALL_IDS: &str = "MATCH (n) RETURN n.id ORDER BY n.id";

#[test]
fn each_scoped_connection_sees_only_what_its_role_allows() {
    let (db, _dir) = store();
    let mut alpha = connected(&db, ALPHA);
    let mut beta = connected(&db, BETA);
    let mut read_key = connected(&db, RD);
    let mut full = connected(&db, FULL);
    assert_eq!(column(&mut alpha, ALL_IDS), ["a1", "a2"]);
    assert_eq!(column(&mut beta, ALL_IDS), ["b1", "n1"]);
    assert_eq!(column(&mut read_key, ALL_IDS), ["a1", "a2"]);
    assert_eq!(
        column(&mut full, ALL_IDS),
        ["a1", "a2", "b1", "n1", "s1"],
        "an unscoped connection is not narrowed"
    );
    // `RETURN n` is narrowed the same way as a projection of it.
    assert_eq!(rows(&mut alpha, "MATCH (n) RETURN n").len(), 2);
    assert_eq!(rows(&mut beta, "MATCH (n) RETURN n").len(), 2);
    assert!(rows(&mut alpha, "MATCH (n:Secret) RETURN n.id").is_empty());
    assert!(rows(&mut beta, "MATCH (n:Doc {id: 'a1'}) RETURN n.id").is_empty());
}

#[test]
fn show_indexes_lists_only_the_labels_a_role_can_see_and_ddl_is_refused() {
    let (db, _dir) = store();
    let mut alpha = connected(&db, ALPHA);
    assert_eq!(column(&mut alpha, "SHOW INDEXES"), ["doc_id"]);
    let mut beta = connected(&db, BETA);
    assert_eq!(column(&mut beta, "SHOW INDEXES"), ["doc_id", "note_id"]);

    for ddl in [
        "CREATE INDEX v FOR (n:Doc) ON (n.v)",
        "CREATE CONSTRAINT c FOR (n:Doc) REQUIRE n.id IS UNIQUE",
    ] {
        let mut alpha = connected(&db, ALPHA);
        let err = failure(&mut alpha, run(ddl));
        assert!(matches!(err, BoltError::Forbidden(_)), "{ddl}: {err:?}");
        assert!(err.message().contains("schema statements"), "{ddl}: {err}");
    }
    let mut full = connected(&db, FULL);
    assert_eq!(
        column(&mut full, "SHOW INDEXES"),
        ["doc_id", "note_id", "secret_id"],
        "nothing was created or dropped"
    );
}

#[test]
fn a_role_transaction_reads_through_its_mask_on_its_begin_snapshot() {
    for (token, want) in [
        (ALPHA, ["a1", "a2"]),
        (BETA, ["b1", "n1"]),
        (RD, ["a1", "a2"]),
    ] {
        let (db, _dir) = store();
        let mut conn = connected(&db, token);
        let out = conn.handle(begin());
        assert!(
            matches!(out.as_slice(), [ServerMessage::Success(_)]),
            "{out:?}"
        );
        // A commit after BEGIN is not in the transaction's snapshot.
        db.submit_batch(vec![
            node("Doc", &format!("late-{token}-a"), "tenant-a"),
            node("Doc", &format!("late-{token}-b"), "tenant-b"),
        ])
        .unwrap();
        assert_eq!(column(&mut conn, ALL_IDS), want, "{token}");
        assert!(rows(&mut conn, "MATCH (n:Secret) RETURN n.id").is_empty());
        let out = conn.handle(commit());
        assert!(
            matches!(out.as_slice(), [ServerMessage::Success(meta)]
                if meta.iter().any(|(k, _)| k == "bookmark")),
            "{out:?}"
        );
    }
}

#[test]
fn a_write_in_a_role_transaction_is_refused_with_the_http_refusal() {
    let (db, _dir) = store();
    let before = ids(&db);
    for (token, refusal) in [
        (ALPHA, TX_ROLE_WRITE_REFUSAL),
        (BETA, TX_ROLE_WRITE_REFUSAL),
        (RD, READ_TIER_REFUSAL),
    ] {
        for write in [
            // Inside alpha's write scope: a transaction still does not write.
            "CREATE (n:Doc {id: 'w1', ns: 'tenant-a'})",
            "MATCH (n:Doc {id: 'a1'}) SET n.v = 1",
            "MATCH (n:Doc {id: 'a2'}) DETACH DELETE n",
            "CREATE INDEX v FOR (n:Doc) ON (n.v)",
        ] {
            let mut conn = connected(&db, token);
            conn.handle(begin());
            let err = failure(&mut conn, run(write));
            assert_eq!(
                err,
                BoltError::Forbidden(refusal.into()),
                "{token}: {write}"
            );
            assert_eq!(conn.state().name(), "Failed");
        }
    }
    assert_eq!(ids(&db), before, "nothing was written");
}

#[test]
fn begin_for_a_role_the_store_does_not_define_is_unauthorized() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut conn = connected(&db, ALPHA);
    let err = failure(&mut conn, begin());
    assert_eq!(err, BoltError::Unauthorized);
}

#[test]
fn an_auto_commit_write_goes_through_the_role_write_scope() {
    let (db, _dir) = store();
    let mut alpha = connected(&db, ALPHA);
    rows(&mut alpha, "CREATE (n:Doc {id: 'c1', ns: 'tenant-a'})");
    assert!(ids(&db).contains(&"c1".to_string()));
    let err = failure(
        &mut alpha,
        run("CREATE (n:Secret {id: 'c2', ns: 'tenant-a'})"),
    );
    assert!(matches!(err, BoltError::Forbidden(_)), "{err:?}");
    let mut beta = connected(&db, BETA);
    let err = failure(&mut beta, run("CREATE (n:Doc {id: 'c3', ns: 'tenant-b'})"));
    assert!(matches!(err, BoltError::Forbidden(_)), "{err:?}");
    let mut read_key = connected(&db, RD);
    let err = failure(
        &mut read_key,
        run("CREATE (n:Doc {id: 'c4', ns: 'tenant-a'})"),
    );
    assert_eq!(err, BoltError::Forbidden(READ_TIER_REFUSAL.into()));
    let all = ids(&db);
    assert!(!all.iter().any(|id| id == "c2" || id == "c3" || id == "c4"));
}

#[test]
fn a_read_mode_begin_for_a_role_still_refuses_with_the_role_text() {
    let (db, _dir) = store();
    let mut alpha = connected(&db, ALPHA);
    let out = alpha.handle(ClientMessage::Other(
        MessageKind::Begin,
        vec![PackValue::Map(vec![(
            "mode".into(),
            PackValue::String("r".into()),
        )])],
    ));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    assert_eq!(column(&mut alpha, ALL_IDS), ["a1", "a2"]);
    let err = failure(&mut alpha, run("CREATE (n:Doc {id: 'r1', ns: 'tenant-a'})"));
    assert_eq!(err, BoltError::Forbidden(TX_ROLE_WRITE_REFUSAL.into()));
}
