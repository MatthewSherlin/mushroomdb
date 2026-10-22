//! Access-key tiers on Bolt (HO-04): `keys.json` tokens resolve to the same
//! tiers the HTTP layer gives them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bolt::connection::Connection;
use bolt::messages::hello::{AuthToken, Hello, Identity, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::PackValue;
use mushroomdb::{AccessKeys, RoleDef, Schema, SharedDb, Value};

const KEYS: &str = r#"{"keys":[
    {"token":"adm-key","tier":"admin"},
    {"token":"wr-key","tier":"write"},
    {"token":"rd-key","tier":"read","role":"viewer"}]}"#;

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
                write: None,
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
    let keys = AccessKeys::from_json(KEYS).unwrap();
    let auth = authenticator(&keys);
    let basic = |token: &str| AuthToken::Basic {
        principal: None,
        credentials: token.into(),
    };
    assert_eq!(
        auth.authenticate(&basic("adm-key")).unwrap(),
        Identity::Full
    );
    assert_eq!(
        auth.authenticate(&basic("wr-key")).unwrap(),
        Identity::Write
    );
    assert_eq!(
        auth.authenticate(&basic("rd-key")).unwrap(),
        Identity::Role("viewer".into())
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
    let keys = AccessKeys::from_json(KEYS).unwrap();
    let mut conn = connected(&db, &keys, "wr-key");

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
    let keys = AccessKeys::from_json(KEYS).unwrap();

    let mut conn = connected(&db, &keys, "rd-key");
    let out = conn.handle(run("MATCH (n:Doc) RETURN COUNT(n)"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn.handle(pull_all());

    let mut conn = connected(&db, &keys, "rd-key");
    let out = conn.handle(run("CREATE (n:Doc {id: 'x'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(docs(&db), Value::Int(1), "nothing was written");

    let mut conn = connected(&db, &keys, "rd-key");
    conn.handle(begin());
    let out = conn.handle(run("CREATE (n:Doc {id: 'y'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(docs(&db), Value::Int(1));
}

#[test]
fn an_unknown_key_is_unauthorized_and_a_reload_changes_who_may_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    std::fs::write(&path, KEYS).unwrap();
    let keys = AccessKeys::load(&path).unwrap();
    let auth = authenticator(&keys);
    let basic = |token: &str| AuthToken::Bearer {
        credentials: token.into(),
    };
    assert_eq!(
        auth.authenticate(&basic("wr-key")).unwrap(),
        Identity::Write
    );

    std::fs::write(&path, r#"{"keys":[{"token":"new-key","tier":"write"}]}"#).unwrap();
    keys.reload().unwrap();
    assert!(auth.authenticate(&basic("wr-key")).is_err());
    assert_eq!(
        auth.authenticate(&basic("new-key")).unwrap(),
        Identity::Write
    );
}

#[test]
fn a_server_with_only_keys_is_not_open_even_after_they_are_all_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("keys.json");
    std::fs::write(&path, KEYS).unwrap();
    let keys = AccessKeys::load(&path).unwrap();
    let auth =
        Arc::new(TokenAuthenticator::new(None, HashMap::new()).with_access_keys(keys.clone()));
    assert!(!auth.is_open());
    std::fs::write(&path, r#"{"keys":[]}"#).unwrap();
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
