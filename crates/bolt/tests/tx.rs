//! BEGIN, COMMIT, ROLLBACK, bookmarks, and RESET during a running batch.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bolt::connection::{serve_connection_with, Connection, ConnectionConfig};
use bolt::framing;
use bolt::messages::hello::{AuthToken, Hello, TokenAuthenticator};
use bolt::messages::route::{RoutingConfig, DRIVER_DEFAULT_DATABASE};
use bolt::messages::run::format_bookmark;
use bolt::messages::{
    ClientMessage, MessageKind, ServerMessage, TAG_BEGIN, TAG_COMMIT, TAG_HELLO, TAG_IGNORED,
    TAG_PULL, TAG_RESET, TAG_RUN, TAG_SUCCESS,
};
use bolt::packstream::{self, PackStruct, PackValue};
use mushroomdb::{BatchOp, SharedDb, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FULL: &str = "full-secret";
const ROLE: &str = "role-secret";
const STORE_NAME: &str = "teststore";

fn authenticator() -> Arc<TokenAuthenticator> {
    let mut roles = HashMap::new();
    roles.insert(ROLE.to_string(), "analyst".to_string());
    Arc::new(TokenAuthenticator::new(Some(FULL.into()), roles))
}

fn routing() -> RoutingConfig {
    RoutingConfig::new(
        "127.0.0.1:7687".parse().unwrap(),
        Some("db.example.test"),
        STORE_NAME,
    )
    .unwrap()
}

fn open_db() -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (SharedDb::open(dir.path()).unwrap(), dir)
}

fn connected_with(db: &SharedDb, token: &str, bookmark_timeout: Duration) -> Connection {
    let mut conn = Connection::new(authenticator(), Duration::from_secs(30), "c".into(), (5, 0))
        .with_routing(Some(routing()))
        .with_database(db.clone(), bookmark_timeout);
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: token.into(),
        },
    }));
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    conn
}

fn connected(db: &SharedDb) -> Connection {
    connected_with(db, FULL, Duration::from_millis(100))
}

fn s(text: &str) -> PackValue {
    PackValue::String(text.into())
}

fn map(entries: Vec<(&str, PackValue)>) -> PackValue {
    PackValue::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn begin_with(extra: Vec<(&str, PackValue)>) -> ClientMessage {
    ClientMessage::Other(MessageKind::Begin, vec![map(extra)])
}

fn begin() -> ClientMessage {
    begin_with(vec![])
}

fn commit() -> ClientMessage {
    ClientMessage::Other(MessageKind::Commit, vec![])
}

fn rollback() -> ClientMessage {
    ClientMessage::Other(MessageKind::Rollback, vec![])
}

fn run_with(query: &str, params: Vec<(&str, PackValue)>) -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Run,
        vec![s(query), map(params), PackValue::Map(vec![])],
    )
}

fn run(query: &str) -> ClientMessage {
    run_with(query, vec![])
}

fn pull_all() -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Pull,
        vec![map(vec![("n", PackValue::Int(-1))])],
    )
}

fn meta(message: &ServerMessage) -> &[(String, PackValue)] {
    match message {
        ServerMessage::Success(meta) => meta,
        other => panic!("expected SUCCESS, got {other:?}"),
    }
}

fn get<'a>(meta: &'a [(String, PackValue)], key: &str) -> Option<&'a PackValue> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn failure_code(out: &[ServerMessage]) -> String {
    match out.last() {
        Some(ServerMessage::Failure(err)) => err.code(),
        other => panic!("expected a FAILURE, got {other:?}"),
    }
}

fn expect_success(out: Vec<ServerMessage>) -> Vec<(String, PackValue)> {
    match out.as_slice() {
        [ServerMessage::Success(meta)] => meta.clone(),
        other => panic!("expected one SUCCESS, got {other:?}"),
    }
}

/// Runs `query` and pulls everything: the rows.
fn rows(conn: &mut Connection, message: ClientMessage) -> Vec<Vec<PackValue>> {
    expect_success(conn.handle(message));
    let out = conn.handle(pull_all());
    let (end, records) = out.split_last().expect("a reply");
    assert!(matches!(end, ServerMessage::Success(_)), "{out:?}");
    records
        .iter()
        .map(|m| match m {
            ServerMessage::Record(values) => values.clone(),
            other => panic!("expected RECORD, got {other:?}"),
        })
        .collect()
}

fn count(conn: &mut Connection, label: &str) -> i64 {
    let out = rows(conn, run(&format!("MATCH (n:{label}) RETURN COUNT(n)")));
    match out.as_slice() {
        [row] => match row.as_slice() {
            [PackValue::Int(n)] => *n,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn count_outside(db: &SharedDb, label: &str) -> Value {
    let rs = db
        .reader()
        .query(
            &format!("MATCH (n:{label}) RETURN COUNT(n)"),
            &BTreeMap::new(),
        )
        .unwrap();
    rs.get(0, "COUNT(n)").cloned().unwrap()
}

fn bookmark_seq(meta: &[(String, PackValue)]) -> u64 {
    match get(meta, "bookmark") {
        Some(PackValue::String(text)) => text
            .strip_prefix("mushroomdb:commit:")
            .expect("bookmark prefix")
            .parse()
            .expect("bookmark number"),
        other => panic!("no bookmark: {other:?}"),
    }
}

// ------------------------------------------------------------ begin / commit

#[test]
fn begin_create_run_commit_makes_the_write_visible() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    assert!(expect_success(conn.handle(begin())).is_empty());
    assert_eq!(conn.state().name(), "InTx");

    expect_success(conn.handle(run("CREATE (n:Pending {id: 'p1', n: 1})")));
    conn.handle(pull_all());
    assert_eq!(count(&mut conn, "Pending"), 1, "sees its own write");
    assert_eq!(
        count_outside(&db, "Pending"),
        Value::Int(0),
        "not committed yet"
    );

    let committed = expect_success(conn.handle(commit()));
    assert_eq!(conn.state().name(), "Ready");
    let seq = bookmark_seq(&committed);
    assert_eq!(
        get(&committed, "bookmark"),
        Some(&PackValue::String(format_bookmark(seq)))
    );
    assert_eq!(seq, db.reader().version, "the bookmark names the commit");
    assert_eq!(count_outside(&db, "Pending"), Value::Int(1));
    assert_eq!(count(&mut conn, "Pending"), 1, "auto-commit sees it too");
}

/// TX-MW (ledger row 144): a driver's managed write transaction sends BEGIN,
/// so "find two nodes, connect them" must run inside it.
#[test]
fn begin_match_create_commit_connects_two_nodes() {
    let (db, _dir) = open_db();
    let node = |key: &str| BatchOp::InsertNode {
        label: "Person".into(),
        key: key.into(),
        props: vec![("id".into(), Value::Str(key.into()))],
    };
    db.submit_batch(vec![node("a"), node("b")]).unwrap();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    let out = rows(
        &mut conn,
        run_with(
            "MATCH (a {id: $a}), (b {id: $b}) CREATE (a)-[:K]->(b)",
            vec![("a", s("a")), ("b", s("b"))],
        ),
    );
    assert_eq!(
        out,
        vec![vec![
            PackValue::Int(0),
            PackValue::Int(0),
            PackValue::Int(0),
            PackValue::Int(1)
        ]]
    );
    let edge = "MATCH (x)-[:K]->(y) RETURN x.id, y.id";
    assert_eq!(rows(&mut conn, run(edge)), vec![vec![s("a"), s("b")]]);
    let outside = |db: &SharedDb| db.reader().query(edge, &BTreeMap::new()).unwrap().len();
    assert_eq!(outside(&db), 0, "not committed yet");
    expect_success(conn.handle(commit()));
    assert_eq!(outside(&db), 1);
}

/// QL-PM: parameters in CREATE and MERGE property maps inside BEGIN … COMMIT.
#[test]
fn begin_create_and_merge_with_parameters_commit() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    rows(
        &mut conn,
        run_with(
            "CREATE (:Pending {id: $id, n: $n})",
            vec![("id", s("p1")), ("n", PackValue::Int(1))],
        ),
    );
    rows(
        &mut conn,
        run_with(
            "MERGE (n:Pending {id: $id}) ON CREATE SET n += $props",
            vec![
                ("id", s("p2")),
                ("props", map(vec![("n", PackValue::Int(2))])),
            ],
        ),
    );
    assert_eq!(count(&mut conn, "Pending"), 2, "sees its own writes");
    assert_eq!(count_outside(&db, "Pending"), Value::Int(0));
    expect_success(conn.handle(commit()));
    assert_eq!(count_outside(&db, "Pending"), Value::Int(2));
    let n = db
        .reader()
        .query(
            "MATCH (n:Pending {id: 'p2'}) RETURN n.n AS n",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(n.get(0, "n"), Some(&Value::Int(2)));
}

#[test]
fn rollback_leaves_nothing_behind() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    expect_success(conn.handle(run("CREATE (n:Gone {id: 'g1'})")));
    conn.handle(pull_all());
    let before = db.reader().version;
    assert!(expect_success(conn.handle(rollback())).is_empty());
    assert_eq!(conn.state().name(), "Ready");
    assert_eq!(count_outside(&db, "Gone"), Value::Int(0));
    assert_eq!(db.reader().version, before, "nothing reached the WAL");
    assert_eq!(count(&mut conn, "Gone"), 0);
}

#[test]
fn a_transaction_that_wrote_nothing_commits_with_the_base_bookmark() {
    let (db, _dir) = open_db();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "Seed".into(),
        key: "s".into(),
        props: vec![],
    }])
    .unwrap();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    assert_eq!(count(&mut conn, "Seed"), 1);
    let before = db.reader().version;
    let committed = expect_success(conn.handle(commit()));
    assert_eq!(bookmark_seq(&committed), before);
    assert_eq!(db.reader().version, before, "no frame written");
}

#[test]
fn a_connection_can_run_one_transaction_after_another() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    for key in ["a", "b"] {
        expect_success(conn.handle(begin()));
        let out = conn.handle(run(&format!("CREATE (n:Seq {{id: '{key}'}})")));
        // qid restarts with each transaction.
        assert_eq!(get(meta(&out[0]), "qid"), Some(&PackValue::Int(0)));
        conn.handle(pull_all());
        expect_success(conn.handle(commit()));
    }
    assert_eq!(count_outside(&db, "Seq"), Value::Int(2));
}

#[test]
fn a_conflicting_commit_is_a_transient_failure_and_reset_recovers() {
    let (db, _dir) = open_db();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "Acct".into(),
        key: "x".into(),
        props: vec![("v".into(), Value::Int(0))],
    }])
    .unwrap();
    let mut first = connected(&db);
    let mut second = connected(&db);
    for conn in [&mut first, &mut second] {
        expect_success(conn.handle(begin()));
        expect_success(conn.handle(run("MATCH (n:Acct {id: 'x'}) SET n.v = 1")));
        conn.handle(pull_all());
    }
    expect_success(first.handle(commit()));
    let out = second.handle(commit());
    assert_eq!(
        failure_code(&out),
        "Neo.TransientError.Transaction.DeadlockDetected"
    );
    assert_eq!(second.state().name(), "Failed");
    expect_success(second.handle(ClientMessage::Reset));
    assert_eq!(second.state().name(), "Ready");
}

#[test]
fn commit_and_rollback_with_an_open_result_are_protocol_violations() {
    for message in [commit(), rollback()] {
        let (db, _dir) = open_db();
        let mut conn = connected(&db);
        expect_success(conn.handle(begin()));
        expect_success(conn.handle(run("CREATE (n:Open {id: 'o'})")));
        assert_eq!(conn.state().name(), "InTxStreaming");
        let out = conn.handle(message);
        assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
        assert!(conn.is_defunct());
        assert_eq!(count_outside(&db, "Open"), Value::Int(0));
    }
}

#[test]
fn commit_and_begin_outside_their_state_are_protocol_violations() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    assert_eq!(
        failure_code(&conn.handle(commit())),
        "Neo.ClientError.Request.Invalid"
    );
    assert!(conn.is_defunct());
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    assert_eq!(
        failure_code(&conn.handle(begin())),
        "Neo.ClientError.Request.Invalid"
    );
    assert!(conn.is_defunct());
}

#[test]
fn a_failed_statement_in_a_transaction_ends_it_and_commit_is_ignored() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    let out = conn.handle(run("THIS IS NOT CYPHER"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Statement.SyntaxError");
    assert_eq!(conn.handle(commit()), vec![ServerMessage::Ignored]);
}

// --------------------------------------------------------------- BEGIN extras

#[test]
fn begin_selects_the_database_through_the_routing_config() {
    let (db, _dir) = open_db();
    for name in [
        PackValue::Null,
        s(""),
        s(DRIVER_DEFAULT_DATABASE),
        s(STORE_NAME),
    ] {
        let mut conn = connected(&db);
        expect_success(conn.handle(begin_with(vec![("db", name.clone())])));
        assert_eq!(conn.state().name(), "InTx", "{name:?}");
        expect_success(conn.handle(rollback()));
    }
    let mut conn = connected(&db);
    let out = conn.handle(begin_with(vec![("db", s("other"))]));
    let by_begin = failure_code(&out);
    assert_eq!(by_begin, "Neo.ClientError.Database.DatabaseNotFound");
    assert_eq!(conn.state().name(), "Failed");

    // The same code a ROUTE for that name gets.
    let mut conn = connected(&db);
    let route = ClientMessage::Other(
        MessageKind::Route,
        vec![map(vec![]), PackValue::List(vec![]), s("other")],
    );
    let by_route = failure_code(&conn.handle(route));
    assert_eq!(by_begin, by_route);
}

#[test]
fn begin_with_no_routing_config_accepts_only_an_absent_database() {
    let (db, _dir) = open_db();
    let mut conn = Connection::new(authenticator(), Duration::from_secs(30), "c".into(), (5, 0))
        .with_database(db.clone(), Duration::from_millis(100));
    conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: FULL.into(),
        },
    }));
    assert_eq!(
        failure_code(&conn.handle(begin_with(vec![("db", s(STORE_NAME))]))),
        "Neo.ClientError.Database.DatabaseNotFound"
    );
}

#[test]
fn begin_rejects_a_malformed_extra_map() {
    let (db, _dir) = open_db();
    let shapes: Vec<ClientMessage> = vec![
        ClientMessage::Other(MessageKind::Begin, vec![]),
        ClientMessage::Other(MessageKind::Begin, vec![s("not a map")]),
        begin_with(vec![("bookmarks", s("mushroomdb:commit:1"))]),
        begin_with(vec![(
            "bookmarks",
            PackValue::List(vec![PackValue::Int(1)]),
        )]),
        begin_with(vec![("bookmarks", PackValue::List(vec![s("foreign:1")]))]),
        begin_with(vec![("tx_timeout", s("soon"))]),
        begin_with(vec![("tx_timeout", PackValue::Int(-5))]),
        begin_with(vec![("tx_metadata", PackValue::Int(1))]),
        begin_with(vec![("mode", s("x"))]),
        begin_with(vec![("mode", PackValue::Int(1))]),
        begin_with(vec![("db", PackValue::Int(1))]),
    ];
    for (index, message) in shapes.into_iter().enumerate() {
        let mut conn = connected(&db);
        let out = conn.handle(message);
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Request.Invalid",
            "shape {index}"
        );
        assert!(conn.is_defunct(), "shape {index}");
    }
}

#[test]
fn begin_accepts_timeout_metadata_and_mode_w() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin_with(vec![
        ("tx_timeout", PackValue::Int(5000)),
        ("tx_metadata", map(vec![("app", s("x"))])),
        ("mode", s("w")),
        ("bookmarks", PackValue::List(vec![])),
    ])));
    assert_eq!(conn.state().name(), "InTx");
}

#[test]
fn impersonation_is_refused() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    let out = conn.handle(begin_with(vec![("imp_user", s("someone"))]));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(conn.state().name(), "Failed");
}

#[test]
fn a_read_mode_transaction_refuses_writes_and_still_reads() {
    let (db, _dir) = open_db();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "Seed".into(),
        key: "s".into(),
        props: vec![],
    }])
    .unwrap();
    let mut conn = connected(&db);
    expect_success(conn.handle(begin_with(vec![("mode", s("r"))])));
    assert_eq!(count(&mut conn, "Seed"), 1);
    let out = conn.handle(run("CREATE (n:Nope {id: 'n'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(count_outside(&db, "Nope"), Value::Int(0));
}

#[test]
fn a_role_token_cannot_write_in_a_transaction() {
    let (db, _dir) = open_db();
    // The role must exist: BEGIN resolves its mask, as HTTP `/tx/begin` does.
    db.write()
        .apply_schema(&mushroomdb::Schema {
            roles: vec![mushroomdb::RoleDef {
                name: "analyst".into(),
                keys: vec![],
                labels: vec!["Role".into()],
                visible_where: None,
                namespaces: None,
                write: Some(mushroomdb::WriteScope {
                    create_labels: vec!["Role".into()],
                    ..Default::default()
                }),
            }],
            ..Default::default()
        })
        .unwrap();
    let mut conn = connected_with(&db, ROLE, Duration::from_millis(100));
    expect_success(conn.handle(begin()));
    let out = conn.handle(run("CREATE (n:Role {id: 'r'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(count_outside(&db, "Role"), Value::Int(0));
}

// ------------------------------------------------------------------ bookmarks

#[test]
fn begin_waits_for_a_bookmark_then_reads_what_it_names() {
    let (db, _dir) = open_db();
    let wanted = db.reader().version + 1;
    let writer_db = db.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        writer_db
            .submit_batch(vec![BatchOp::InsertNode {
                label: "Causal".into(),
                key: "c1".into(),
                props: vec![],
            }])
            .unwrap();
    });
    let mut conn = connected_with(&db, FULL, Duration::from_secs(10));
    let started = Instant::now();
    expect_success(conn.handle(begin_with(vec![(
        "bookmarks",
        PackValue::List(vec![s(&format_bookmark(wanted))]),
    )])));
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "BEGIN waited for the bookmark"
    );
    assert_eq!(count(&mut conn, "Causal"), 1, "the transaction reads it");
    writer.join().unwrap();
}

#[test]
fn a_commit_on_one_connection_is_the_bookmark_for_another() {
    let (db, _dir) = open_db();
    let mut a = connected(&db);
    expect_success(a.handle(begin()));
    expect_success(a.handle(run("CREATE (n:Hand {id: 'h'})")));
    a.handle(pull_all());
    let bookmark = get(&expect_success(a.handle(commit())), "bookmark")
        .cloned()
        .unwrap();
    let mut b = connected(&db);
    expect_success(b.handle(begin_with(vec![(
        "bookmarks",
        PackValue::List(vec![bookmark]),
    )])));
    assert_eq!(count(&mut b, "Hand"), 1);
}

#[test]
fn begin_times_out_waiting_for_a_bookmark_with_a_transient_failure() {
    let (db, _dir) = open_db();
    let mut conn = connected(&db);
    let out = conn.handle(begin_with(vec![(
        "bookmarks",
        PackValue::List(vec![s(&format_bookmark(db.reader().version + 100))]),
    )]));
    assert_eq!(
        failure_code(&out),
        "Neo.TransientError.General.DatabaseUnavailable"
    );
    assert_eq!(conn.state().name(), "Failed");
}

// ---------------------------------------------------------------- null params

#[test]
fn a_null_parameter_is_null_not_absent() {
    let (db, _dir) = open_db();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "Seed".into(),
        key: "s".into(),
        props: vec![],
    }])
    .unwrap();
    let mut conn = connected(&db);
    // One row when `$p` is null; none when it is bound to a value.
    let query = "MATCH (n:Seed) WHERE $p IS NULL RETURN COUNT(n) AS c";
    let row =
        |value: PackValue, conn: &mut Connection| rows(conn, run_with(query, vec![("p", value)]));
    assert_eq!(
        row(PackValue::Null, &mut conn),
        vec![vec![PackValue::Int(1)]]
    );
    assert_eq!(
        row(PackValue::Int(1), &mut conn),
        vec![vec![PackValue::Int(0)]]
    );

    expect_success(conn.handle(begin()));
    assert_eq!(
        row(PackValue::Null, &mut conn),
        vec![vec![PackValue::Int(1)]]
    );
}

// ------------------------------------------------------- RESET on the wire

const OFFER: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04,
    0x00, 0x00, 0x00, 0x03,
];

fn framed_message(tag: u8, fields: Vec<PackValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    packstream::pack(&PackValue::Struct(PackStruct::new(tag, fields)), &mut buf);
    let mut out = Vec::new();
    framing::frame(&buf, &mut out);
    out
}

async fn read_message<R: AsyncReadExt + Unpin>(stream: &mut R) -> PackStruct {
    let mut bytes = Vec::new();
    loop {
        match framing::deframe(&bytes) {
            Ok((message, _)) => {
                let (value, _) = packstream::unpack(&message).unwrap();
                let PackValue::Struct(s) = value else {
                    panic!("not a structure")
                };
                return s;
            }
            Err(_) => {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
            }
        }
    }
}

type Client = tokio::io::DuplexStream;

async fn wire(
    db: &SharedDb,
    bookmark_timeout: Duration,
) -> (Client, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (mut client, server) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(serve_connection_with(
        server,
        authenticator(),
        "wire".into(),
        ConnectionConfig {
            routing: Some(routing()),
            bookmark_timeout,
            ..ConnectionConfig::default()
        },
        Some(db.clone()),
    ));
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    let extra = map(vec![("scheme", s("basic")), ("credentials", s(FULL))]);
    client
        .write_all(&framed_message(TAG_HELLO, vec![extra]))
        .await
        .unwrap();
    assert_eq!(read_message(&mut client).await.tag, TAG_SUCCESS);
    (client, task)
}

fn future_bookmark(db: &SharedDb) -> PackValue {
    map(vec![(
        "bookmarks",
        PackValue::List(vec![s(&format_bookmark(db.reader().version + 1000))]),
    )])
}

async fn next_within(client: &mut Client) -> PackStruct {
    tokio::time::timeout(Duration::from_secs(5), read_message(client))
        .await
        .expect("a reply within five seconds: RESET did not interrupt the wait")
}

#[tokio::test]
async fn reset_ends_a_bookmark_wait_in_begin() {
    let (db, _dir) = open_db();
    let (mut client, task) = wire(&db, Duration::from_secs(60)).await;
    client
        .write_all(&framed_message(TAG_BEGIN, vec![future_bookmark(&db)]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .write_all(&framed_message(TAG_RESET, vec![]))
        .await
        .unwrap();
    assert_eq!(next_within(&mut client).await.tag, TAG_IGNORED, "BEGIN");
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS, "RESET");
    // The connection is usable again.
    client
        .write_all(&framed_message(TAG_BEGIN, vec![map(vec![])]))
        .await
        .unwrap();
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS);
    client
        .write_all(&framed_message(TAG_COMMIT, vec![]))
        .await
        .unwrap();
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS);
    drop(client);
    let _ = task.await;
}

#[tokio::test]
async fn reset_ends_a_bookmark_wait_in_run_and_ignores_what_was_queued_behind_it() {
    let (db, _dir) = open_db();
    let (mut client, task) = wire(&db, Duration::from_secs(60)).await;
    let mut queued = framed_message(
        TAG_RUN,
        vec![s("RETURN 1"), map(vec![]), future_bookmark(&db)],
    );
    queued.extend(framed_message(
        TAG_PULL,
        vec![map(vec![("n", PackValue::Int(-1))])],
    ));
    client.write_all(&queued).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .write_all(&framed_message(TAG_RESET, vec![]))
        .await
        .unwrap();
    assert_eq!(next_within(&mut client).await.tag, TAG_IGNORED, "RUN");
    assert_eq!(next_within(&mut client).await.tag, TAG_IGNORED, "PULL");
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS, "RESET");
    drop(client);
    let _ = task.await;
}

#[tokio::test]
async fn reset_waits_for_a_statement_already_running_and_then_takes_effect() {
    let (db, _dir) = open_db();
    let (mut client, task) = wire(&db, Duration::from_secs(60)).await;
    // The write lock is held, so the auto-commit write below blocks inside
    // the engine, where nothing can interrupt it.
    let guard = db.write();
    client
        .write_all(&framed_message(
            TAG_RUN,
            vec![s("CREATE (n:Late {id: 'l'})"), map(vec![]), map(vec![])],
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .write_all(&framed_message(TAG_RESET, vec![]))
        .await
        .unwrap();
    // Nothing is answered while the statement is stuck.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), read_message(&mut client))
            .await
            .is_err()
    );
    drop(guard);
    // The statement finished and committed; its reply goes out, and then the
    // RESET is applied.
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS, "RUN");
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS, "RESET");
    assert_eq!(count_outside(&db, "Late"), Value::Int(1));
    // And the connection is Ready: a new statement runs.
    client
        .write_all(&framed_message(
            TAG_RUN,
            vec![s("RETURN 1"), map(vec![]), map(vec![])],
        ))
        .await
        .unwrap();
    assert_eq!(next_within(&mut client).await.tag, TAG_SUCCESS);
    drop(client);
    let _ = task.await;
}

#[tokio::test]
async fn a_disconnect_during_a_bookmark_wait_ends_the_connection_promptly() {
    let (db, _dir) = open_db();
    let (mut client, task) = wire(&db, Duration::from_secs(60)).await;
    client
        .write_all(&framed_message(TAG_BEGIN, vec![future_bookmark(&db)]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the connection ends without waiting out the bookmark")
        .unwrap()
        .unwrap();
}

/// 0.10 TX-21: a unique-constraint clash found at COMMIT is the client error
/// `ConstraintValidationFailed`, not an internal or transient one, and the
/// transaction's writes do not land.
#[test]
fn a_constraint_violation_at_commit_is_a_constraint_validation_failure() {
    let (db, _dir) = open_db();
    db.write()
        .create_constraint("uniq_name", "Person", "name")
        .unwrap();
    let mut holder = connected(&db);
    expect_success(holder.handle(run("CREATE (:Person {id: 'holder', name: 'taken'})")));
    holder.handle(pull_all());

    let mut conn = connected(&db);
    expect_success(conn.handle(begin()));
    expect_success(conn.handle(run("CREATE (:Person {id: 'dup', name: 'taken'})")));
    let pulled = conn.handle(pull_all());
    assert!(
        matches!(pulled.last(), Some(ServerMessage::Success(_))),
        "the clash is not judged while buffered: {pulled:?}"
    );
    let out = conn.handle(commit());
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Schema.ConstraintValidationFailed"
    );
    match out.last() {
        Some(ServerMessage::Failure(err)) => {
            assert!(err.message().contains("uniq_name"), "{}", err.message())
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(conn.state().name(), "Failed");
    expect_success(conn.handle(ClientMessage::Reset));
    assert_eq!(count(&mut conn, "Person"), 1);
}
