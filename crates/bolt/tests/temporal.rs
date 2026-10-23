//! Dates, times, durations and bytes over Bolt against a real store: a value
//! sent as a `RUN` parameter is stored, survives a reopen, and comes back
//! through `RUN`/`PULL` exactly as it was sent, at each Bolt version the
//! server speaks.

use std::sync::Arc;
use std::time::Duration;

use bolt::connection::Connection;
use bolt::messages::hello::{AuthToken, Hello, TokenAuthenticator};
use bolt::messages::{ClientMessage, MessageKind, ServerMessage};
use bolt::packstream::{PackStruct, PackValue};
use mushroomdb::{SharedDb, Value};

const VERSIONS: [(u8, u8); 2] = [(5, 0), (4, 4)];

fn connected(db: &SharedDb, version: (u8, u8)) -> Connection {
    let mut conn = Connection::new(
        Arc::new(TokenAuthenticator::new(None, Default::default())),
        Duration::from_secs(30),
        "c".into(),
        version,
    )
    .with_database(db.clone(), Duration::from_millis(100));
    let out = conn.handle(ClientMessage::Hello(Hello {
        user_agent: None,
        auth: AuthToken::None,
    }));
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    conn
}

fn int(n: i64) -> PackValue {
    PackValue::Int(n)
}

fn st(tag: u8, fields: Vec<PackValue>) -> PackValue {
    PackValue::Struct(PackStruct::new(tag, fields))
}

fn run_with(query: &str, params: Vec<(&str, PackValue)>) -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Run,
        vec![
            PackValue::String(query.into()),
            PackValue::Map(params.into_iter().map(|(k, v)| (k.into(), v)).collect()),
            PackValue::Map(vec![]),
        ],
    )
}

fn simple(kind: MessageKind) -> ClientMessage {
    ClientMessage::Other(kind, vec![PackValue::Map(vec![])])
}

fn pull_all() -> ClientMessage {
    ClientMessage::Other(
        MessageKind::Pull,
        vec![PackValue::Map(vec![("n".into(), int(-1))])],
    )
}

/// Runs `query` and pulls every row; panics on any FAILURE.
fn rows(conn: &mut Connection, query: &str, params: Vec<(&str, PackValue)>) -> Vec<Vec<PackValue>> {
    let out = conn.handle(run_with(query, params));
    assert!(
        matches!(out.as_slice(), [ServerMessage::Success(_)]),
        "{query}: {out:?}"
    );
    let out = conn.handle(pull_all());
    let (end, records) = out.split_last().unwrap();
    assert!(matches!(end, ServerMessage::Success(_)), "{query}: {end:?}");
    records
        .iter()
        .map(|m| match m {
            ServerMessage::Record(values) => values.clone(),
            other => panic!("expected RECORD, got {other:?}"),
        })
        .collect()
}

/// One wire value per supported type, in the form `version` uses.
fn samples(version: (u8, u8)) -> Vec<(&'static str, PackValue, Value)> {
    // 2024-01-15T12:34:56.789012+05:30.
    let seconds_utc = 1_705_322_096;
    let offset = 19_800;
    let date_time = if version >= (5, 0) {
        st(0x49, vec![int(seconds_utc), int(789_012_000), int(offset)])
    } else {
        st(
            0x46,
            vec![int(seconds_utc + offset), int(789_012_000), int(offset)],
        )
    };
    vec![
        ("date", st(0x44, vec![int(19_737)]), Value::Date(19_737)),
        (
            "local",
            st(0x64, vec![int(seconds_utc), int(789_012_000)]),
            Value::LocalDateTime(seconds_utc * 1_000_000 + 789_012),
        ),
        (
            "before_epoch",
            st(0x64, vec![int(-1), int(999_999_000)]),
            Value::LocalDateTime(-1),
        ),
        (
            "instant",
            date_time,
            Value::DateTime {
                micros_utc: seconds_utc * 1_000_000 + 789_012,
                offset_seconds: offset as i32,
            },
        ),
        (
            "duration",
            st(0x45, vec![int(14), int(3), int(14_706), int(789_000_001)]),
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 14_706_789_000_001,
            },
        ),
        (
            "bytes",
            PackValue::Bytes(vec![0, 1, 254, 255]),
            Value::Binary(vec![0, 1, 254, 255]),
        ),
        (
            "large_bytes",
            PackValue::Bytes(vec![0xAB; 70_000]),
            Value::Binary(vec![0xAB; 70_000]),
        ),
    ]
}

/// Creates node `key` and sets its `v` from the parameter `wire`, returning
/// the rows of the `SET` statement (`tail` may add a `RETURN`). A property
/// map in `CREATE` takes literals only, so the value goes through `SET`.
fn store(conn: &mut Connection, key: &str, wire: PackValue, tail: &str) -> Vec<Vec<PackValue>> {
    rows(conn, &format!("CREATE (n:T {{id: '{key}'}})"), vec![]);
    rows(
        conn,
        &format!("MATCH (n:T) WHERE n.id = '{key}' SET n.v = $v{tail}"),
        vec![("v", wire)],
    )
}

#[test]
fn every_type_survives_param_store_reopen_and_read_back_at_each_version() {
    for version in VERSIONS {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = SharedDb::open(dir.path()).unwrap();
            assert!(!db.read().is_v11());
            let mut conn = connected(&db, version);
            for (key, wire, engine) in samples(version) {
                // The value echoed by the write statement itself.
                let echoed = store(&mut conn, key, wire.clone(), " RETURN n.v");
                assert_eq!(echoed, vec![vec![wire.clone()]], "{key} at {version:?}");
                // Stored as the engine value.
                let seen = db
                    .reader()
                    .query(
                        "MATCH (n:T) WHERE n.id = $k RETURN n.v",
                        &[("k".to_string(), Value::Str(key.into()))].into(),
                    )
                    .unwrap();
                assert_eq!(seen.get(0, "n.v"), Some(&engine), "{key} at {version:?}");
            }
            // The first temporal write moved the store to the V11 format (TY-12).
            assert!(db.read().is_v11());
        }
        // After a reopen, read back over a fresh connection.
        let db = SharedDb::open(dir.path()).unwrap();
        let mut conn = connected(&db, version);
        for (key, wire, _) in samples(version) {
            let back = rows(
                &mut conn,
                "MATCH (n:T) WHERE n.id = $k RETURN n.v",
                vec![("k", PackValue::String(key.into()))],
            );
            assert_eq!(back, vec![vec![wire]], "{key} at {version:?} after reopen");
        }
    }
}

#[test]
fn temporal_parameters_work_inside_an_explicit_transaction() {
    for version in VERSIONS {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut conn = connected(&db, version);
        let out = conn.handle(simple(MessageKind::Begin));
        assert!(
            matches!(out.as_slice(), [ServerMessage::Success(_)]),
            "{out:?}"
        );
        for (key, wire, _) in samples(version) {
            store(&mut conn, key, wire, "");
        }
        let out = conn.handle(ClientMessage::Other(MessageKind::Commit, vec![]));
        assert!(
            matches!(out.as_slice(), [ServerMessage::Success(_)]),
            "{out:?}"
        );
        for (key, wire, _) in samples(version) {
            let back = rows(
                &mut conn,
                "MATCH (n:T) WHERE n.id = $k RETURN n.v",
                vec![("k", PackValue::String(key.into()))],
            );
            assert_eq!(back, vec![vec![wire]], "{key} at {version:?}");
        }
    }
}

#[test]
fn a_temporal_parameter_compares_with_a_stored_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut conn = connected(&db, (5, 0));
    for days in [19_000, 19_500, 20_000] {
        store(&mut conn, &days.to_string(), st(0x44, vec![int(days)]), "");
    }
    let found = rows(
        &mut conn,
        "MATCH (n:T) WHERE n.v > $since RETURN COUNT(n)",
        vec![("since", st(0x44, vec![int(19_400)]))],
    );
    assert_eq!(found, vec![vec![int(2)]]);
}

fn failure(out: &[ServerMessage]) -> (String, String) {
    match out.last() {
        Some(ServerMessage::Failure(err)) => (err.code(), err.message()),
        other => panic!("expected a FAILURE, got {other:?}"),
    }
}

#[test]
fn refused_parameters_fail_the_request_and_store_nothing() {
    let refused: [(PackValue, &str); 5] = [
        (st(0x64, vec![int(0), int(1)]), "sub-microsecond"),
        (
            st(
                0x69,
                vec![int(0), int(0), PackValue::String("Europe/Paris".into())],
            ),
            "named time zone",
        ),
        (st(0x54, vec![int(0), int(0)]), "Time"),
        (
            st(0x44, vec![PackValue::String("2024-01-15".into())]),
            "malformed",
        ),
        (st(0x46, vec![int(0), int(0), int(0)]), "0x46"),
    ];
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut conn = connected(&db, (5, 0));
    rows(&mut conn, "CREATE (n:Bad {id: 'b'})", vec![]);
    for (wire, needle) in refused {
        let out = conn.handle(run_with(
            "MATCH (n:Bad) SET n.v = $v RETURN n.v",
            vec![("v", wire)],
        ));
        let (code, message) = failure(&out);
        assert_eq!(code, "Neo.ClientError.Request.Invalid");
        assert!(message.contains(needle), "{message}");
        // The connection recovers with RESET, as after any request failure.
        let out = conn.handle(ClientMessage::Reset);
        assert!(
            matches!(out.as_slice(), [ServerMessage::Success(_)]),
            "{out:?}"
        );
    }
    let count = rows(
        &mut conn,
        "MATCH (n:Bad) WHERE n.v IS NULL RETURN COUNT(n)",
        vec![],
    );
    assert_eq!(count, vec![vec![int(1)]], "the node kept no value");
}
