//! RUN, PULL and DISCARD against a real store.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bolt::connection::{
    serve_connection_with, Connection, ConnectionConfig, OpenTransaction, ResultCursor,
};
use bolt::error::BoltError;
use bolt::framing;
use bolt::messages::hello::{AuthToken, Hello, Identity, TokenAuthenticator};
use bolt::messages::run::{format_bookmark, QueryService};
use bolt::messages::{
    ClientMessage, MessageKind, ServerMessage, TAG_FAILURE, TAG_GOODBYE, TAG_HELLO, TAG_PULL,
    TAG_RECORD, TAG_RUN, TAG_SUCCESS,
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

fn service(db: &SharedDb) -> Arc<QueryService> {
    // Accepts what a driver sends when it names no database, and the store's
    // own name; the real check comes from the routing config.
    let check = Arc::new(|requested: Option<&str>| match requested {
        None | Some("") | Some(STORE_NAME) => Ok(()),
        Some(other) => Err(BoltError::DatabaseNotFound(format!(
            "no database {other:?}"
        ))),
    });
    Arc::new(QueryService::new(db.clone(), check).with_bookmark_timeout(Duration::from_millis(100)))
}

fn open_db(_label: &str) -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (SharedDb::open(dir.path()).unwrap(), dir)
}

fn connected(db: &SharedDb, token: &str) -> Connection {
    let mut conn = Connection::new(authenticator(), Duration::from_secs(30), "c".into(), (5, 0))
        .with_query_service(service(db));
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

fn s(text: &str) -> PackValue {
    PackValue::String(text.into())
}

fn run_message(query: &str, extra: Vec<(String, PackValue)>) -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Run,
        vec![s(query), PackValue::Map(vec![]), PackValue::Map(extra)],
    )
}

fn run(query: &str) -> ClientMessage {
    run_message(query, vec![])
}

fn stream_message(kind: MessageKind, n: i64, qid: Option<i64>) -> ClientMessage {
    let mut extra = vec![("n".to_string(), PackValue::Int(n))];
    if let Some(qid) = qid {
        extra.push(("qid".to_string(), PackValue::Int(qid)));
    }
    ClientMessage::Other(kind, vec![PackValue::Map(extra)])
}

fn pull(n: i64) -> ClientMessage {
    stream_message(MessageKind::Pull, n, None)
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

/// The rows and the closing message of a `PULL`/`DISCARD` reply.
fn rows_and_end(out: &[ServerMessage]) -> (Vec<&Vec<PackValue>>, &ServerMessage) {
    let (end, records) = out.split_last().expect("a reply");
    let rows = records
        .iter()
        .map(|m| match m {
            ServerMessage::Record(values) => values,
            other => panic!("expected RECORD, got {other:?}"),
        })
        .collect();
    (rows, end)
}

fn seed_rows(db: &SharedDb, count: i64) {
    let ops = (0..count)
        .map(|i| BatchOp::InsertNode {
            label: "Row".into(),
            key: format!("row{i}"),
            props: vec![("n".into(), Value::Int(i))],
        })
        .collect();
    db.submit_batch(ops).unwrap();
}

fn count_rows(conn: &mut Connection, query: &str) -> i64 {
    assert!(matches!(
        conn.handle(run(query)).as_slice(),
        [ServerMessage::Success(_)]
    ));
    let out = conn.handle(pull(-1));
    let (rows, _) = rows_and_end(&out);
    match rows[0][0] {
        PackValue::Int(n) => n,
        ref other => panic!("not an integer: {other:?}"),
    }
}

// ------------------------------------------------------------ auto-commit

#[test]
fn run_then_pull_all_returns_the_count() {
    let (db, _dir) = open_db("count");
    seed_rows(&db, 3);
    let mut conn = connected(&db, FULL);
    let out = conn.handle(run("MATCH (n) RETURN COUNT(n)"));
    let [header] = out.as_slice() else {
        panic!("{out:?}")
    };
    let fields = get(meta(header), "fields").unwrap();
    assert_eq!(fields, &PackValue::List(vec![s("COUNT(n)")]));
    assert!(matches!(
        get(meta(header), "t_first"),
        Some(PackValue::Int(_))
    ));
    assert!(get(meta(header), "qid").is_none(), "auto-commit has no qid");
    assert_eq!(conn.state().name(), "Streaming");

    let out = conn.handle(pull(-1));
    let (rows, end) = rows_and_end(&out);
    assert_eq!(rows, vec![&vec![PackValue::Int(3)]]);
    let end = meta(end);
    assert_eq!(get(end, "type"), Some(&s("r")));
    assert!(get(end, "has_more").is_none());
    assert!(matches!(get(end, "t_last"), Some(PackValue::Int(_))));
    assert!(
        matches!(get(end, "bookmark"), Some(PackValue::String(b)) if b.starts_with("mushroomdb:commit:"))
    );
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn auto_commit_create_is_durable_and_visible_to_the_next_statement() {
    let (db, _dir) = open_db("create");
    let mut conn = connected(&db, FULL);
    assert_eq!(count_rows(&mut conn, "MATCH (n:Made) RETURN COUNT(n)"), 0);
    let out = conn.handle(run("CREATE (n:Made {id: 'm1', n: 1})"));
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    let out = conn.handle(pull(-1));
    let (_, end) = rows_and_end(&out);
    assert_eq!(get(meta(end), "type"), Some(&s("w")));
    assert_eq!(count_rows(&mut conn, "MATCH (n:Made) RETURN COUNT(n)"), 1);
    // Visible to a reader that did not go through the connection.
    let seen = db
        .reader()
        .query("MATCH (n:Made) RETURN COUNT(n)", &BTreeMap::new())
        .unwrap();
    assert_eq!(seen.get(0, "COUNT(n)"), Some(&Value::Int(1)));
}

#[test]
fn a_write_bookmark_is_at_least_the_commit_and_a_later_run_may_wait_on_it() {
    let (db, _dir) = open_db("bookmark");
    let mut conn = connected(&db, FULL);
    conn.handle(run("CREATE (n:Made {id: 'b1'})"));
    let out = conn.handle(pull(-1));
    let (_, end) = rows_and_end(&out);
    let Some(PackValue::String(bookmark)) = get(meta(end), "bookmark").cloned() else {
        panic!("no bookmark")
    };
    let wait = run_message(
        "MATCH (n:Made) RETURN COUNT(n)",
        vec![("bookmarks".into(), PackValue::List(vec![s(&bookmark)]))],
    );
    assert!(matches!(
        conn.handle(wait).as_slice(),
        [ServerMessage::Success(_)]
    ));
}

#[test]
fn a_bookmark_the_store_never_reaches_times_out_as_a_transient_failure() {
    let (db, _dir) = open_db("bookmark-timeout");
    let mut conn = connected(&db, FULL);
    let ahead = db.reader().version + 1_000;
    let started = Instant::now();
    let out = conn.handle(run_message(
        "MATCH (n) RETURN COUNT(n)",
        vec![(
            "bookmarks".into(),
            PackValue::List(vec![s(&format_bookmark(ahead))]),
        )],
    ));
    assert_eq!(
        failure_code(&out),
        "Neo.TransientError.General.DatabaseUnavailable"
    );
    // The configured wait is 100 ms; the bound is loose because the machine is.
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(conn.state().name(), "Failed");
}

#[test]
fn a_foreign_bookmark_is_a_malformed_request() {
    let (db, _dir) = open_db("bookmark-foreign");
    let mut conn = connected(&db, FULL);
    let out = conn.handle(run_message(
        "MATCH (n) RETURN COUNT(n)",
        vec![("bookmarks".into(), PackValue::List(vec![s("elsewhere:7")]))],
    ));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
    assert!(conn.is_defunct());
}

#[test]
fn a_thousand_rows_stream_as_a_hundred_then_nine_hundred() {
    let (db, _dir) = open_db("stream");
    seed_rows(&db, 1000);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n AS n"));

    let out = conn.handle(pull(100));
    let (rows, end) = rows_and_end(&out);
    assert_eq!(rows.len(), 100);
    assert_eq!(get(meta(end), "has_more"), Some(&PackValue::Bool(true)));
    assert!(get(meta(end), "bookmark").is_none());
    assert_eq!(conn.state().name(), "Streaming");

    let out = conn.handle(pull(900));
    let (rest, end) = rows_and_end(&out);
    assert_eq!(rest.len(), 900);
    assert!(get(meta(end), "has_more").is_none());
    assert!(get(meta(end), "bookmark").is_some());
    assert_eq!(conn.state().name(), "Ready");

    let mut seen: Vec<i64> = rows
        .iter()
        .chain(rest.iter())
        .map(|row| match row[0] {
            PackValue::Int(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen,
        (0..1000).collect::<Vec<_>>(),
        "no row twice, none lost"
    );
}

#[test]
fn pull_of_exactly_the_remaining_rows_ends_the_stream() {
    let (db, _dir) = open_db("exact");
    seed_rows(&db, 5);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let out = conn.handle(pull(5));
    let (rows, end) = rows_and_end(&out);
    assert_eq!(rows.len(), 5);
    assert!(get(meta(end), "has_more").is_none());
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn discard_drops_rows_and_a_partial_discard_leaves_the_rest() {
    let (db, _dir) = open_db("discard");
    seed_rows(&db, 10);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let out = conn.handle(stream_message(MessageKind::Discard, 4, None));
    let [end] = out.as_slice() else {
        panic!("DISCARD sends no records: {out:?}")
    };
    assert_eq!(get(meta(end), "has_more"), Some(&PackValue::Bool(true)));
    let out = conn.handle(pull(-1));
    assert_eq!(rows_and_end(&out).0.len(), 6);

    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let out = conn.handle(stream_message(MessageKind::Discard, -1, None));
    let [end] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert!(get(meta(end), "has_more").is_none());
    assert!(get(meta(end), "bookmark").is_some());
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn a_zero_or_negative_n_other_than_minus_one_fails_the_request() {
    let (db, _dir) = open_db("badn");
    seed_rows(&db, 2);
    for n in [0, -2] {
        let mut conn = connected(&db, FULL);
        conn.handle(run("MATCH (n:Row) RETURN n.n"));
        let out = conn.handle(pull(n));
        assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
        assert_eq!(conn.state().name(), "Failed", "n = {n}");
    }
}

#[test]
fn a_syntax_error_fails_the_run_and_reset_recovers() {
    let (db, _dir) = open_db("syntax");
    let mut conn = connected(&db, FULL);
    let out = conn.handle(run("MATCH (n RETURN"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Statement.SyntaxError");
    assert_eq!(conn.state().name(), "Failed");
    assert_eq!(conn.handle(pull(-1)), vec![ServerMessage::Ignored]);
    conn.handle(ClientMessage::Reset);
    assert_eq!(count_rows(&mut conn, "MATCH (n) RETURN COUNT(n)"), 0);
}

#[test]
fn reset_mid_stream_drops_the_cursor() {
    let (db, _dir) = open_db("reset");
    seed_rows(&db, 10);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    conn.handle(pull(3));
    conn.handle(ClientMessage::Reset);
    assert_eq!(conn.state().name(), "Ready");
    assert_eq!(count_rows(&mut conn, "MATCH (n:Row) RETURN COUNT(n)"), 10);
}

#[test]
fn run_parameters_reach_the_query() {
    let (db, _dir) = open_db("params");
    seed_rows(&db, 10);
    let mut conn = connected(&db, FULL);
    let message = ClientMessage::Other(
        MessageKind::Run,
        vec![
            s("MATCH (n:Row) WHERE n.n = $want RETURN n.n"),
            PackValue::Map(vec![("want".into(), PackValue::Int(7))]),
            PackValue::Map(vec![]),
        ],
    );
    conn.handle(message);
    let out = conn.handle(pull(-1));
    assert_eq!(rows_and_end(&out).0, vec![&vec![PackValue::Int(7)]]);
}

#[test]
fn null_parameters_are_left_out_like_the_http_route_does() {
    let (db, _dir) = open_db("nullparam");
    seed_rows(&db, 5);
    let mut conn = connected(&db, FULL);
    let message = ClientMessage::Other(
        MessageKind::Run,
        vec![
            s("MATCH (n:Row) WHERE n.n = $want RETURN n.n"),
            PackValue::Map(vec![
                ("want".into(), PackValue::Int(2)),
                ("optional".into(), PackValue::Null),
                (
                    "nested".into(),
                    PackValue::Map(vec![("gone".into(), PackValue::Null)]),
                ),
            ]),
            PackValue::Map(vec![]),
        ],
    );
    let out = conn.handle(message);
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    let out = conn.handle(pull(-1));
    assert_eq!(rows_and_end(&out).0, vec![&vec![PackValue::Int(2)]]);
}

#[test]
fn a_structure_parameter_is_a_client_error_not_a_server_fault() {
    let (db, _dir) = open_db("structparam");
    let mut conn = connected(&db, FULL);
    let message = ClientMessage::Other(
        MessageKind::Run,
        vec![
            s("MATCH (n) RETURN COUNT(n)"),
            PackValue::Map(vec![(
                "d".into(),
                PackValue::Struct(PackStruct::new(0x44, vec![PackValue::Int(1)])),
            )]),
            PackValue::Map(vec![]),
        ],
    );
    let out = conn.handle(message);
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
}

#[test]
fn union_works_in_an_auto_commit_read() {
    let (db, _dir) = open_db("union");
    seed_rows(&db, 3);
    db.submit_batch(
        [1, 5]
            .into_iter()
            .map(|n| BatchOp::InsertNode {
                label: "Other".into(),
                key: format!("other{n}"),
                props: vec![("n".into(), Value::Int(n))],
            })
            .collect(),
    )
    .unwrap();
    let mut conn = connected(&db, FULL);
    let out = conn.handle(run(
        "MATCH (a:Row) RETURN a.n AS x UNION MATCH (b:Other) RETURN b.n AS x",
    ));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    let out = conn.handle(pull(-1));
    let mut seen: Vec<i64> = rows_and_end(&out)
        .0
        .iter()
        .map(|row| match row[0] {
            PackValue::Int(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 1, 2, 5], "UNION removes the duplicate 1");
}

// ------------------------------------------------------------- databases

#[test]
fn database_selection_follows_the_check() {
    let (db, _dir) = open_db("dbname");
    for accepted in [
        vec![],
        vec![("db".to_string(), PackValue::Null)],
        vec![("db".to_string(), s(""))],
        vec![("db".to_string(), s(STORE_NAME))],
    ] {
        let mut conn = connected(&db, FULL);
        let out = conn.handle(run_message("MATCH (n) RETURN COUNT(n)", accepted));
        assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    }
    let mut conn = connected(&db, FULL);
    let out = conn.handle(run_message(
        "MATCH (n) RETURN COUNT(n)",
        vec![("db".into(), s("elsewhere"))],
    ));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Database.DatabaseNotFound"
    );
    assert_eq!(conn.state().name(), "Failed");
}

// ---------------------------------------------------------- role tokens

fn count_in(db: &SharedDb, label: &str) -> Value {
    db.reader()
        .query(
            &format!("MATCH (n:{label}) RETURN COUNT(n)"),
            &BTreeMap::new(),
        )
        .unwrap()
        .get(0, "COUNT(n)")
        .cloned()
        .unwrap()
}

/// A store with the role `analyst` (reads `Public`, may create `Public`), two
/// `Public` nodes and three `Secret` ones.
fn db_with_role(label: &str) -> (SharedDb, tempfile::TempDir) {
    use mushroomdb::{RoleDef, Schema, WriteScope};
    let (db, dir) = open_db(label);
    let node = |label: &str, i: i64| BatchOp::InsertNode {
        label: label.into(),
        key: format!("{label}{i}"),
        props: vec![("n".into(), Value::Int(i))],
    };
    let mut ops: Vec<BatchOp> = (0..2).map(|i| node("Public", i)).collect();
    ops.extend((0..3).map(|i| node("Secret", i)));
    db.submit_batch(ops).unwrap();
    let role = RoleDef {
        name: "analyst".into(),
        keys: vec![],
        labels: vec!["Public".into()],
        visible_where: None,
        namespaces: None,
        write: Some(WriteScope {
            create_labels: vec!["Public".into()],
            ..WriteScope::default()
        }),
    };
    db.write()
        .apply_schema(&Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![role],
        })
        .unwrap();
    (db, dir)
}

#[test]
fn a_role_token_reads_only_what_its_mask_shows() {
    let (db, _dir) = db_with_role("role-read");
    let mut full = connected(&db, FULL);
    assert_eq!(count_rows(&mut full, "MATCH (n) RETURN COUNT(n)"), 5);
    let mut conn = connected(&db, ROLE);
    assert_eq!(conn.identity(), Some(&Identity::Role("analyst".into())));
    assert_eq!(count_rows(&mut conn, "MATCH (n) RETURN COUNT(n)"), 2);
    assert_eq!(count_rows(&mut conn, "MATCH (n:Secret) RETURN COUNT(n)"), 0);
}

#[test]
fn a_role_token_write_inside_its_scope_works_and_outside_is_forbidden() {
    let (db, _dir) = db_with_role("role-write");
    let mut conn = connected(&db, ROLE);
    let out = conn.handle(run("CREATE (n:Public {id: 'p9', n: 9})"));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{out:?}"
    );
    conn.handle(pull(-1));
    assert_eq!(count_in(&db, "Public"), Value::Int(3));

    let out = conn.handle(run("CREATE (n:Secret {id: 's9', n: 9})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    assert_eq!(
        count_in(&db, "Secret"),
        Value::Int(3),
        "nothing was written"
    );
    assert_eq!(conn.state().name(), "Failed");
}

#[test]
fn a_role_token_with_no_role_definition_is_unauthorized_for_reads_and_writes() {
    let (db, _dir) = open_db("role-undefined");
    let mut conn = connected(&db, ROLE);
    let out = conn.handle(run("MATCH (n) RETURN COUNT(n)"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Unauthorized");
    conn.handle(ClientMessage::Reset);
    let out = conn.handle(run("CREATE (n:X {id: 'r1'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Unauthorized");
    assert_eq!(count_in(&db, "X"), Value::Int(0));
}

// -------------------------------------------------------- in a transaction

fn in_tx(db: &SharedDb, token: &str) -> Connection {
    let mut conn = connected(db, token);
    let tx: Box<dyn OpenTransaction> = Box::new(db.begin_transaction());
    assert!(conn.enter_tx(tx, Instant::now()).is_ok());
    conn
}

#[test]
fn run_in_a_transaction_reads_its_own_writes_and_commits_nothing() {
    let (db, _dir) = open_db("tx");
    let mut conn = in_tx(&db, FULL);
    let out = conn.handle(run("CREATE (n:Pending {id: 'p1', n: 1})"));
    let [header] = out.as_slice() else {
        panic!("{out:?}")
    };
    let qid = match get(meta(header), "qid") {
        Some(PackValue::Int(qid)) => *qid,
        other => panic!("a transaction RUN carries a qid: {other:?}"),
    };
    assert_eq!(conn.state().name(), "InTxStreaming");
    let out = conn.handle(stream_message(MessageKind::Pull, -1, Some(qid)));
    let (_, end) = rows_and_end(&out);
    assert!(
        get(meta(end), "bookmark").is_none(),
        "the commit carries it"
    );
    assert_eq!(conn.state().name(), "InTx");

    assert_eq!(
        count_rows(&mut conn, "MATCH (n:Pending) RETURN COUNT(n)"),
        1
    );
    let outside = db
        .reader()
        .query("MATCH (n:Pending) RETURN COUNT(n)", &BTreeMap::new())
        .unwrap();
    assert_eq!(outside.get(0, "COUNT(n)"), Some(&Value::Int(0)));

    conn.handle(ClientMessage::Reset);
    let after = db
        .reader()
        .query("MATCH (n:Pending) RETURN COUNT(n)", &BTreeMap::new())
        .unwrap();
    assert_eq!(
        after.get(0, "COUNT(n)"),
        Some(&Value::Int(0)),
        "rolled back"
    );
}

#[test]
fn a_transaction_streams_in_pieces_like_auto_commit() {
    let (db, _dir) = open_db("tx-stream");
    seed_rows(&db, 250);
    let mut conn = in_tx(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let out = conn.handle(pull(100));
    assert_eq!(rows_and_end(&out).0.len(), 100);
    assert_eq!(conn.state().name(), "InTxStreaming");
    let out = conn.handle(pull(-1));
    assert_eq!(rows_and_end(&out).0.len(), 150);
    assert_eq!(conn.state().name(), "InTx");
}

fn qid_of(out: &[ServerMessage]) -> i64 {
    match out {
        [header] => match get(meta(header), "qid") {
            Some(PackValue::Int(qid)) => *qid,
            other => panic!("no qid: {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn several_results_stay_open_in_a_transaction_and_pull_by_qid() {
    let (db, _dir) = open_db("tx-many");
    seed_rows(&db, 10);
    let mut conn = in_tx(&db, FULL);
    let first = qid_of(&conn.handle(run("MATCH (n:Row) RETURN n.n")));
    let second = qid_of(&conn.handle(run("MATCH (n:Row) RETURN n.n AS m")));
    assert_ne!(first, second);
    let pull_qid = |conn: &mut Connection, n, qid| {
        conn.handle(stream_message(MessageKind::Pull, n, Some(qid)))
    };

    let out = pull_qid(&mut conn, 3, first);
    assert_eq!(rows_and_end(&out).0.len(), 3);
    let out = pull_qid(&mut conn, 2, second);
    assert_eq!(rows_and_end(&out).0.len(), 2);
    let out = pull_qid(&mut conn, -1, first);
    let (rows, end) = rows_and_end(&out);
    assert_eq!(rows.len(), 7, "the first result kept its own position");
    assert!(get(meta(end), "has_more").is_none());
    assert_eq!(
        conn.state().name(),
        "InTxStreaming",
        "the second is still open"
    );
    let out = pull_qid(&mut conn, -1, second);
    assert_eq!(rows_and_end(&out).0.len(), 8);
    assert_eq!(conn.state().name(), "InTx");
}

#[test]
fn qid_minus_one_means_the_latest_open_result() {
    let (db, _dir) = open_db("tx-latest");
    seed_rows(&db, 4);
    let mut conn = in_tx(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    conn.handle(run("MATCH (n:Row) RETURN COUNT(n)"));
    let out = conn.handle(pull(-1));
    assert_eq!(rows_and_end(&out).0, vec![&vec![PackValue::Int(4)]]);
    let out = conn.handle(pull(-1));
    assert_eq!(
        rows_and_end(&out).0.len(),
        4,
        "now the earlier one is latest"
    );
    assert_eq!(conn.state().name(), "InTx");
}

#[test]
fn discard_by_qid_leaves_the_other_result_open() {
    let (db, _dir) = open_db("tx-discard");
    seed_rows(&db, 4);
    let mut conn = in_tx(&db, FULL);
    let first = qid_of(&conn.handle(run("MATCH (n:Row) RETURN n.n")));
    conn.handle(run("MATCH (n:Row) RETURN n.n AS m"));
    let out = conn.handle(stream_message(MessageKind::Discard, -1, Some(first)));
    assert_eq!(out.len(), 1);
    assert_eq!(conn.state().name(), "InTxStreaming");
}

#[test]
fn a_qid_that_no_longer_exists_is_a_client_error() {
    let (db, _dir) = open_db("tx-gone");
    seed_rows(&db, 4);
    let mut conn = in_tx(&db, FULL);
    let first = qid_of(&conn.handle(run("MATCH (n:Row) RETURN n.n")));
    conn.handle(run("MATCH (n:Row) RETURN n.n AS m"));
    conn.handle(stream_message(MessageKind::Pull, -1, Some(first)));
    // `first` is drained and gone; the second result is still open.
    let out = conn.handle(stream_message(MessageKind::Pull, -1, Some(first)));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
    // The Bolt state machine moves any FAILURE to FAILED, which ends the
    // transaction; RESET recovers.
    assert_eq!(conn.state().name(), "Failed");
    conn.handle(ClientMessage::Reset);
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn a_pull_naming_another_qid_fails() {
    let (db, _dir) = open_db("tx-qid");
    let mut conn = in_tx(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let out = conn.handle(stream_message(MessageKind::Pull, -1, Some(12345)));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
}

#[test]
fn a_role_token_cannot_run_in_a_transaction() {
    let (db, _dir) = open_db("tx-role");
    let mut conn = in_tx(&db, ROLE);
    let out = conn.handle(run("CREATE (n:Sneak {id: 's1'})"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Forbidden");
    let seen = db
        .reader()
        .query("MATCH (n:Sneak) RETURN COUNT(n)", &BTreeMap::new())
        .unwrap();
    assert_eq!(seen.get(0, "COUNT(n)"), Some(&Value::Int(0)));
}

#[test]
fn a_transaction_syntax_error_leaves_nothing_buffered_and_fails_the_connection() {
    let (db, _dir) = open_db("tx-syntax");
    let mut conn = in_tx(&db, FULL);
    let out = conn.handle(run("CREATE (n:"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Statement.SyntaxError");
    assert_eq!(conn.state().name(), "Failed");
}

// -------------------------------------------------- rows the wire cannot carry

#[test]
fn a_row_holding_a_structure_over_65535_fields_cannot_be_encoded() {
    // No engine value is a structure, so the case is a hand-built row.
    let wide = PackValue::Struct(PackStruct::new(
        0x44,
        vec![PackValue::Null; bolt::messages::MAX_STRUCT_FIELDS + 1],
    ));
    let record = ServerMessage::Record(vec![wide]);
    assert!(matches!(record.encode(), Err(BoltError::TooLarge(_))));
    // And the unchecked path never emits a half frame.
    let bytes = record.encode_or_failure();
    let (value, _) = packstream::unpack(&bytes).unwrap();
    let PackValue::Struct(s) = value else {
        panic!()
    };
    assert_eq!(s.tag, TAG_FAILURE);
}

#[test]
fn a_record_encodes_as_a_one_field_structure() {
    let bytes = ServerMessage::Record(vec![PackValue::Int(1), PackValue::Null])
        .encode()
        .unwrap();
    let (value, _) = packstream::unpack(&bytes).unwrap();
    assert_eq!(
        value,
        PackValue::Struct(PackStruct::new(
            TAG_RECORD,
            vec![PackValue::List(vec![PackValue::Int(1), PackValue::Null])]
        ))
    );
}

// ------------------------------------------------------------ idle, default

#[test]
fn an_idle_auto_commit_result_is_closed_like_an_idle_transaction() {
    let (db, _dir) = open_db("idle-cursor");
    seed_rows(&db, 10);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    conn.handle(pull(2));
    // The connection's idle limit is the 30 s it was built with.
    assert!(!conn.check_timeout(Instant::now() + Duration::from_secs(29)));
    assert!(conn.check_timeout(Instant::now() + Duration::from_secs(31)));
    assert_eq!(conn.state().name(), "Failed");
    let out = conn.handle(pull(-1));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
    conn.handle(ClientMessage::Reset);
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn activity_keeps_an_auto_commit_result_open() {
    let (db, _dir) = open_db("busy-cursor");
    seed_rows(&db, 10);
    let mut conn = connected(&db, FULL);
    conn.handle(run("MATCH (n:Row) RETURN n.n"));
    let later = Instant::now() + Duration::from_secs(25);
    let out = conn.handle_at(pull(2), later);
    assert_eq!(rows_and_end(&out).0.len(), 2);
    assert!(!conn.check_timeout(later + Duration::from_secs(25)));
}

#[test]
fn the_database_check_sees_none_when_the_client_names_no_database() {
    use std::sync::Mutex;
    let (db, _dir) = open_db("default-db");
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = seen.clone();
    let check = Arc::new(move |requested: Option<&str>| {
        record.lock().unwrap().push(requested.map(str::to_string));
        Ok(())
    });
    let service = Arc::new(QueryService::new(db.clone(), check));
    let mut conn = Connection::new(authenticator(), Duration::from_secs(30), "c".into(), (5, 0))
        .with_query_service(service);
    conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::Basic {
            principal: None,
            credentials: FULL.into(),
        },
    }));
    conn.handle(run("MATCH (n) RETURN COUNT(n)"));
    conn.handle(pull(-1));
    conn.handle(run_message(
        "MATCH (n) RETURN COUNT(n)",
        vec![("db".into(), s("named"))],
    ));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![None, Some("named".to_string())],
        "an absent db reaches the check as None, the default path"
    );
}

// ----------------------------------------------------------------- the wire

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

#[tokio::test]
async fn wire_connect_hello_run_pull_goodbye() {
    let (db, _dir) = open_db("wire");
    seed_rows(&db, 1000);
    let (mut client, server) = tokio::io::duplex(1 << 22);
    let task = tokio::spawn(serve_connection_with(
        server,
        authenticator(),
        "wire".into(),
        ConnectionConfig::default(),
        Some(service(&db)),
    ));
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [0, 0, 0, 5]);

    let extra = PackValue::Map(vec![
        ("scheme".into(), s("basic")),
        ("credentials".into(), s(FULL)),
    ]);
    client
        .write_all(&framed_message(TAG_HELLO, vec![extra]))
        .await
        .unwrap();
    assert_eq!(read_message(&mut client).await.tag, TAG_SUCCESS);

    // A count, as a driver sends it: RUN then PULL in one write.
    let mut both = framed_message(
        TAG_RUN,
        vec![
            s("MATCH (n:Row) RETURN COUNT(n)"),
            PackValue::Map(vec![]),
            PackValue::Map(vec![]),
        ],
    );
    both.extend(framed_message(
        TAG_PULL,
        vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
    ));
    client.write_all(&both).await.unwrap();
    assert_eq!(read_message(&mut client).await.tag, TAG_SUCCESS);
    let record = read_message(&mut client).await;
    assert_eq!(record.tag, TAG_RECORD);
    assert_eq!(
        record.fields,
        vec![PackValue::List(vec![PackValue::Int(1000)])]
    );
    assert_eq!(read_message(&mut client).await.tag, TAG_SUCCESS);

    // Streaming 1000 rows as 100 then 900.
    client
        .write_all(&framed_message(
            TAG_RUN,
            vec![
                s("MATCH (n:Row) RETURN n.n"),
                PackValue::Map(vec![]),
                PackValue::Map(vec![]),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(read_message(&mut client).await.tag, TAG_SUCCESS);
    for (n, expect) in [(100, 100usize), (900, 900)] {
        client
            .write_all(&framed_message(
                TAG_PULL,
                vec![PackValue::Map(vec![("n".into(), PackValue::Int(n))])],
            ))
            .await
            .unwrap();
        for _ in 0..expect {
            assert_eq!(read_message(&mut client).await.tag, TAG_RECORD);
        }
        let end = read_message(&mut client).await;
        assert_eq!(end.tag, TAG_SUCCESS);
        let PackValue::Map(end_meta) = &end.fields[0] else {
            panic!()
        };
        assert_eq!(
            end_meta.iter().any(|(k, _)| k == "has_more"),
            n == 100,
            "has_more only while rows remain"
        );
    }

    client
        .write_all(&framed_message(TAG_GOODBYE, vec![]))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn trait_objects_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Connection>();
    assert_send::<Box<dyn ResultCursor>>();
    assert_send::<Box<dyn OpenTransaction>>();
}
