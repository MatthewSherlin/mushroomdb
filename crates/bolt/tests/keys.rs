//! Access-key tiers on Bolt (HO-04): `keys.json` tokens resolve to the same
//! tiers the HTTP layer gives them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bolt::connection::Connection;
use bolt::messages::hello::{AuthToken, Hello, Identity, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::PackValue;
use mushroomdb::{AccessKeys, BatchOp, RoleDef, Schema, SharedDb, Value, WriteScope};

const ADM: &str = "adm-key-0123456789abcdef0123456789abcdef";
const WR: &str = "wr-key-0123456789abcdef0123456789abcdef";
const RD: &str = "rd-key-0123456789abcdef0123456789abcdef";
const NEW: &str = "new-key-0123456789abcdef0123456789abcdef";

/// A key file's text: hashes only.
fn keys_text(entries: &[(&str, &str, Option<&str>)]) -> String {
    let keys: Vec<String> = entries
        .iter()
        .map(|(token, tier, role)| {
            let role = role.map_or(String::new(), |r| format!(r#","role":"{r}""#));
            format!(
                r#"{{"hash":"{}","tier":"{tier}"{role}}}"#,
                mushroomdb::hash_token(token).unwrap()
            )
        })
        .collect();
    format!(r#"{{"keys":[{}]}}"#, keys.join(","))
}

fn keys_json() -> String {
    keys_text(&[
        (ADM, "admin", None),
        (WR, "write", None),
        (RD, "read", Some("viewer")),
    ])
}

/// Writes `text` as a private file, as the CLI does.
fn write_private(path: &std::path::Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn authenticator(keys: &AccessKeys) -> Arc<TokenAuthenticator> {
    Arc::new(
        TokenAuthenticator::new(Some("env-token".into()), HashMap::new())
            .with_access_keys(keys.clone()),
    )
}

fn hello(token: &str) -> ClientMessage {
    ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: token.into(),
        },
    })
}

fn db_with_viewer() -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write()
        .apply_schema(&Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![RoleDef {
                name: "viewer".into(),
                keys: vec![],
                labels: vec!["Doc".into()],
                visible_where: None,
                namespaces: None,
                // A scope the `read` key must still not get to use.
                write: Some(WriteScope {
                    create_labels: vec!["Doc".into()],
                    ..WriteScope::default()
                }),
            }],
        })
        .unwrap();
    (db, dir)
}

fn connected(db: &SharedDb, keys: &AccessKeys, token: &str) -> Connection {
    let mut conn = Connection::new(
        authenticator(keys),
        Duration::from_secs(30),
        "c".into(),
        (5, 0),
    )
    .with_database(db.clone(), Duration::from_millis(100));
    let out = conn.handle(hello(token));
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

fn failure_code(out: &[ServerMessage]) -> String {
    match out.last() {
        Some(ServerMessage::Failure(err)) => err.code(),
        other => panic!("expected a FAILURE, got {other:?}"),
    }
}

fn docs(db: &SharedDb) -> Value {
    db.reader()
        .query("MATCH (n:Doc) RETURN COUNT(n)", &BTreeMap::new())
        .unwrap()
        .get(0, "COUNT(n)")
        .cloned()
        .unwrap()
}

#[test]
fn each_key_tier_resolves_to_its_identity() {
    let keys = AccessKeys::from_json(&keys_json()).unwrap();
    let auth = authenticator(&keys);
    let basic = |token: &str| AuthToken::Basic {
        principal: None,
        credentials: token.into(),
    };
    assert_eq!(auth.authenticate(&basic(ADM)).unwrap(), Identity::Full);
    assert_eq!(auth.authenticate(&basic(WR)).unwrap(), Identity::Write);
    assert_eq!(
        auth.authenticate(&basic(RD)).unwrap(),
        Identity::ReadOnlyRole("viewer".into())
    );
    assert_eq!(
        auth.authenticate(&basic("env-token")).unwrap(),
        Identity::Full
    );
    assert!(auth.authenticate(&basic("nope")).is_err());
}

#[test]
fn a_write_key_writes_in_auto_commit_and_in_a_transaction() {
    let (db, _dir) = db_with_viewer();
    let keys = AccessKeys::from_json(&keys_json()).unwrap();
    let mut conn = connected(&db, &keys, WR);

    let out = conn.handle(run("CREATE (n:Doc {id: 'a'})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn.handle(pull_all());
    assert_eq!(docs(&db), Value::Int(1));

    let out = conn.handle(begin());
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    let out = conn.handle(run("CREATE (n:Doc {id: 'b'})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn.handle(pull_all());
    let out = conn.handle(ClientMessage::Other(MessageKind::Commit, vec![]));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    assert_eq!(docs(&db), Value::Int(2));
}

#[test]
fn a_read_key_reads_but_cannot_write() {
    let (db, _dir) = db_with_viewer();
    db.write()
        .insert_node(
            "Doc",
            "seed",
            vec![("id".into(), Value::Str("seed".into()))],
        )
        .unwrap();
    let keys = AccessKeys::from_json(&keys_json()).unwrap();

    let mut conn = connected(&db, &keys, RD);
    let out = conn.handle(run("MATCH (n:Doc) RETURN COUNT(n)"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn.handle(pull_all());

    // The role's own write scope must not reach a read key, by any statement.
    for statement in [
        "CREATE (n:Doc {id: 'x'})",
        "MATCH (n:Doc {id: 'seed'}) SET n.age = 1",
        "MATCH (n:Doc {id: 'seed'}) DETACH DELETE n",
        "MERGE (n:Doc {id: 'm'})",
    ] {
        let mut conn = connected(&db, &keys, RD);
        let out = conn.handle(run(statement));
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Security.Forbidden",
            "{statement}"
        );
        assert_eq!(docs(&db), Value::Int(1), "{statement} wrote");

        let mut conn = connected(&db, &keys, RD);
        conn.handle(begin());
        let out = conn.handle(run(statement));
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Security.Forbidden",
            "in a transaction: {statement}"
        );
        assert_eq!(
            docs(&db),
            Value::Int(1),
            "{statement} wrote in a transaction"
        );
    }
}

#[test]
fn an_unknown_key_is_unauthorized_and_a_reload_changes_who_may_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let keys = AccessKeys::load(&path).unwrap();
    let auth = authenticator(&keys);
    let basic = |token: &str| AuthToken::Bearer {
        credentials: token.into(),
    };
    assert_eq!(auth.authenticate(&basic(WR)).unwrap(), Identity::Write);

    write_private(&path, &keys_text(&[(NEW, "write", None)]));
    keys.reload().unwrap();
    assert!(auth.authenticate(&basic(WR)).is_err());
    assert_eq!(auth.authenticate(&basic(NEW)).unwrap(), Identity::Write);
}

#[test]
fn a_server_with_only_keys_is_not_open_even_after_they_are_all_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let keys = AccessKeys::load(&path).unwrap();
    let auth =
        Arc::new(TokenAuthenticator::new(None, HashMap::new()).with_access_keys(keys.clone()));
    assert!(!auth.is_open());
    write_private(&path, r#"{"keys":[]}"#);
    keys.reload().unwrap();
    assert!(
        !auth.is_open(),
        "no keys left must lock the door, not open it"
    );
    assert!(auth.authenticate(&AuthToken::None).is_err());
}

#[test]
fn a_server_with_no_tokens_and_no_keys_is_open() {
    let keys = AccessKeys::empty();
    let auth = TokenAuthenticator::new(None, HashMap::new()).with_access_keys(keys);
    assert!(auth.is_open());
    assert_eq!(auth.authenticate(&AuthToken::None).unwrap(), Identity::Full);
}

// --- Row 142: revoked / changed keys on an open Bolt connection ---

/// Load a keys.json file, connect with `token`, and return both.
fn connected_with_file(
    db: &SharedDb,
    path: &std::path::Path,
    token: &str,
) -> (Connection, AccessKeys) {
    let keys = AccessKeys::load(path).unwrap();
    let conn = connected(db, &keys, token);
    (conn, keys)
}

#[test]
fn row_142_a_revoked_key_is_refused_on_its_next_run() {
    // Establish a connection with the write key, verify a statement runs,
    // then revoke the key via a reload, and assert the next RUN fails.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Statement runs before revocation.
    let out = conn.handle(run("RETURN 1"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "before revocation: {out:?}"
    );
    conn.handle(pull_all());

    // Remove WR from the key file and reload.
    write_private(
        &path,
        &keys_text(&[(ADM, "admin", None), (RD, "read", Some("viewer"))]),
    );
    keys.reload().unwrap();

    // The next RUN must fail with Unauthorized (key revoked).
    let out = conn.handle(run("RETURN 2"));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Unauthorized",
        "after revocation: {out:?}"
    );
}

#[test]
fn row_142_a_revoked_key_is_refused_on_its_next_begin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Revoke WR.
    write_private(&path, &keys_text(&[(ADM, "admin", None)]));
    keys.reload().unwrap();

    // BEGIN must fail with Unauthorized.
    let out = conn.handle(begin());
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Unauthorized",
        "BEGIN after revocation: {out:?}"
    );
}

#[test]
fn row_142_a_key_whose_tier_changes_to_read_is_refused_on_writes() {
    // WR key is demoted from write to read (for the viewer role). After
    // reload the connection must see the new, narrower tier: reads succeed
    // and writes are refused with Forbidden (not Unauthorized).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();

    // Seed a Doc node so the read key has something to query.
    db.write()
        .insert_node(
            "Doc",
            "seed142",
            vec![("id".into(), Value::Str("seed142".into()))],
        )
        .unwrap();

    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Write succeeds before the change.
    let out = conn.handle(run("CREATE (n:Doc {id: 'before'})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "write before tier change: {out:?}"
    );
    conn.handle(pull_all());

    // Change WR from write tier to read/viewer tier and reload.
    write_private(
        &path,
        &keys_text(&[
            (ADM, "admin", None),
            (WR, "read", Some("viewer")),
            (RD, "read", Some("viewer")),
        ]),
    );
    keys.reload().unwrap();

    // A read succeeds with the new (narrowed) identity.
    let out = conn.handle(run("MATCH (n:Doc) RETURN COUNT(n)"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "read after tier change: {out:?}"
    );
    conn.handle(pull_all());

    // A write is now Forbidden (not Unauthorized) because the key still
    // exists but its tier no longer allows writes.
    let out = conn.handle(run("CREATE (n:Doc {id: 'after'})"));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Forbidden",
        "write after tier change: {out:?}"
    );
}

#[test]
fn row_142_a_transaction_open_when_its_key_is_revoked_fails_on_its_next_run() {
    // Begin a transaction, revoke the key mid-transaction, then assert the
    // next RUN inside the transaction is refused (Unauthorized) and the
    // connection moves to Failed — meaning the transaction is rolled back.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Open a transaction.
    let out = conn.handle(begin());
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "BEGIN: {out:?}"
    );

    // Revoke WR while the transaction is open.
    write_private(&path, &keys_text(&[(ADM, "admin", None)]));
    keys.reload().unwrap();

    // RUN inside the open transaction must fail with Unauthorized.
    let out = conn.handle(run("CREATE (n:Doc {id: 'revoked-tx'})"));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Unauthorized",
        "RUN in tx after key revocation: {out:?}"
    );

    // Verify the node was not written (transaction rolled back by the failure).
    assert!(
        !db.read().has_node("revoked-tx"),
        "revoked node must not have been committed"
    );
}

#[test]
fn row_142_an_unrelated_key_on_another_connection_is_unaffected() {
    // Revoking one key must not affect connections holding other keys.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let keys = AccessKeys::load(&path).unwrap();

    let mut admin_conn = connected(&db, &keys, ADM);
    let mut wr_conn = connected(&db, &keys, WR);

    // Both work before the reload.
    assert!(matches!(
        admin_conn.handle(run("RETURN 1")).as_slice(),
        [ServerMessage::Success(_)]
    ));
    admin_conn.handle(pull_all());
    assert!(matches!(
        wr_conn.handle(run("RETURN 2")).as_slice(),
        [ServerMessage::Success(_)]
    ));
    wr_conn.handle(pull_all());

    // Remove only WR.
    write_private(
        &path,
        &keys_text(&[(ADM, "admin", None), (RD, "read", Some("viewer"))]),
    );
    keys.reload().unwrap();

    // ADM connection is still alive and works.
    let out = admin_conn.handle(run("RETURN 3"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "ADM connection after WR revoked: {out:?}"
    );
    admin_conn.handle(pull_all());

    // WR connection is now refused.
    let out = wr_conn.handle(run("RETURN 4"));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Unauthorized",
        "WR connection after revocation: {out:?}"
    );
}

/// A write that ran while the key was valid is NOT committed if the key is
/// revoked between `RUN` and `COMMIT`. `COMMIT` re-checks the key and rolls
/// back the transaction when the check fails.
#[test]
fn row_142_a_committed_write_after_key_revocation_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Open a transaction and run a write (recheck passes — key still valid).
    let out = conn.handle(begin());
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "BEGIN: {out:?}"
    );
    let out = conn.handle(run("CREATE (n:Doc {id: 'should-not-persist'})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "RUN: {out:?}"
    );
    conn.handle(pull_all());

    // Revoke the key after the write has run but before COMMIT.
    write_private(&path, &keys_text(&[(ADM, "admin", None)]));
    keys.reload().unwrap();

    // COMMIT is re-checked: the revoked key is refused with Unauthorized.
    let out = conn.handle(ClientMessage::Other(MessageKind::Commit, vec![]));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Unauthorized",
        "COMMIT after revocation: {out:?}"
    );

    // Confirm the write did not persist. Use a fresh connection with the admin
    // key, which was not revoked.
    let mut verify_conn = connected(&db, &keys, ADM);
    let out = verify_conn.handle(run(
        "MATCH (n:Doc {id: 'should-not-persist'}) RETURN COUNT(n)",
    ));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "verify RUN: {out:?}"
    );
    let out = verify_conn.handle(pull_all());
    // COUNT(n) must be 0: the node was rolled back.
    assert!(
        matches!(out.last(), Some(ServerMessage::Success(_))),
        "verify PULL: {out:?}"
    );
    assert_eq!(
        docs(&db),
        Value::Int(0),
        "write committed by the revoked key must NOT persist"
    );
}

/// A write transaction whose key is downgraded to read between `RUN` and
/// `COMMIT` is refused at `COMMIT` with `Forbidden`, and the writes do not
/// persist.
#[test]
fn row_142_a_committed_write_after_key_downgrade_to_read_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    write_private(&path, &keys_json());
    let (db, _dir2) = db_with_viewer();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // Open a transaction and run a write (key is still write-tier).
    let out = conn.handle(begin());
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "BEGIN: {out:?}"
    );
    let out = conn.handle(run("CREATE (n:Doc {id: 'downgrade-should-not-persist'})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "RUN: {out:?}"
    );
    conn.handle(pull_all());

    // Downgrade WR from write to read/viewer tier (key still exists, but lower tier).
    write_private(
        &path,
        &keys_text(&[
            (ADM, "admin", None),
            (WR, "read", Some("viewer")),
            (RD, "read", Some("viewer")),
        ]),
    );
    keys.reload().unwrap();

    // COMMIT is re-checked: the downgraded key is refused with Forbidden.
    let out = conn.handle(ClientMessage::Other(MessageKind::Commit, vec![]));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Forbidden",
        "COMMIT after tier downgrade: {out:?}"
    );

    // Confirm the write did not persist.
    assert_eq!(
        docs(&db),
        Value::Int(0),
        "write by the downgraded key must NOT persist"
    );
}

// --- Row 142: role change on an open scoped transaction ---

/// A database with two roles bound to different namespaces, used to test
/// whether a role change mid-transaction leaks the original role's view.
fn db_with_alpha_beta_roles() -> (SharedDb, tempfile::TempDir) {
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
                    write: None,
                },
                RoleDef {
                    name: "beta".into(),
                    keys: vec![],
                    labels: vec!["Doc".into()],
                    visible_where: None,
                    namespaces: Some(vec!["tenant-b".into()]),
                    write: None,
                },
            ],
            ..Default::default()
        })
        .unwrap();
    db.submit_batch(vec![
        BatchOp::InsertNode {
            label: "Doc".into(),
            key: "alpha-doc".into(),
            props: vec![
                ("id".into(), Value::Str("alpha-doc".into())),
                ("ns".into(), Value::Str("tenant-a".into())),
            ],
        },
        BatchOp::InsertNode {
            label: "Doc".into(),
            key: "beta-doc".into(),
            props: vec![
                ("id".into(), Value::Str("beta-doc".into())),
                ("ns".into(), Value::Str("tenant-b".into())),
            ],
        },
    ])
    .unwrap();
    (db, dir)
}

/// A `keys.json` key whose role changes (alpha → beta) while a scoped
/// transaction is open must not return alpha-visible data on the next `RUN`.
/// The correct behaviour is to fail closed: the changed identity means the
/// transaction was opened under a different view, so the connection refuses
/// further statements until the client resets and starts a new transaction.
#[test]
fn row_142_a_changed_role_in_an_open_transaction_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    // Key starts as role alpha.
    write_private(
        &path,
        &keys_text(&[(WR, "read", Some("alpha")), (ADM, "admin", None)]),
    );
    let (db, _dir2) = db_with_alpha_beta_roles();
    let (mut conn, keys) = connected_with_file(&db, &path, WR);

    // BEGIN: identity is ReadOnlyRole("alpha"), snapshot sees alpha's docs.
    let out = conn.handle(begin());
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "BEGIN: {out:?}"
    );

    // Change the key's role from alpha to beta and reload.
    write_private(
        &path,
        &keys_text(&[(WR, "read", Some("beta")), (ADM, "admin", None)]),
    );
    keys.reload().unwrap();

    // The next RUN inside the transaction must be refused: the identity
    // changed and the transaction was opened under a different view.
    // It must NOT return "alpha-doc" (which beta cannot see).
    let out = conn.handle(run("MATCH (n:Doc) RETURN n.id"));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Security.Forbidden",
        "RUN after role change in open tx: {out:?}"
    );
}
