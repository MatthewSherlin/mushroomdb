//! Connection actor: auth, RESET, GOODBYE, state machine, read loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bolt::connection::{
    serve_connection, Connection, ConnectionConfig, OpenTransaction, ResultCursor,
};
use bolt::error::BoltError;
use bolt::framing;
use bolt::messages::hello::{Identity, TokenAuthenticator};
use bolt::messages::{
    ClientMessage, MessageKind, ServerMessage, TAG_FAILURE, TAG_GOODBYE, TAG_HELLO, TAG_IGNORED,
    TAG_PULL, TAG_RESET, TAG_RUN, TAG_SUCCESS,
};
use bolt::packstream::{self, PackStruct, PackValue};
use mushroomdb::{BatchOp, SharedDb, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FULL: &str = "full-secret";
const ROLE: &str = "role-secret";

fn authenticator() -> Arc<TokenAuthenticator> {
    let mut roles = HashMap::new();
    roles.insert(ROLE.to_string(), "analyst".to_string());
    Arc::new(TokenAuthenticator::new(Some(FULL.into()), roles))
}

fn connection(timeout: Duration) -> Connection {
    Connection::new(authenticator(), timeout, "bolt-1".into(), (5, 0))
}

fn hello(scheme: &str, credentials: &str) -> ClientMessage {
    ClientMessage::decode(&hello_bytes(scheme, credentials)).unwrap()
}

fn hello_bytes(scheme: &str, credentials: &str) -> Vec<u8> {
    let extra = PackValue::Map(vec![
        ("user_agent".into(), PackValue::String("test/1".into())),
        ("scheme".into(), PackValue::String(scheme.into())),
        ("principal".into(), PackValue::String("ignored".into())),
        ("credentials".into(), PackValue::String(credentials.into())),
    ]);
    message_bytes(TAG_HELLO, vec![extra])
}

fn message_bytes(tag: u8, fields: Vec<PackValue>) -> Vec<u8> {
    let mut buf = Vec::new();
    packstream::pack(&PackValue::Struct(PackStruct::new(tag, fields)), &mut buf);
    buf
}

fn other(kind: MessageKind) -> ClientMessage {
    ClientMessage::Other(kind, vec![])
}

fn ready() -> Connection {
    let mut conn = connection(Duration::from_secs(30));
    let out = conn.handle(hello("basic", FULL));
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    conn
}

fn failure_code(out: &[ServerMessage]) -> String {
    match out {
        [ServerMessage::Failure(err)] => err.code(),
        other => panic!("expected one FAILURE, got {other:?}"),
    }
}

struct FakeTx {
    rolled_back: Arc<AtomicBool>,
    _ops: Vec<BatchOp>,
}

impl OpenTransaction for FakeTx {
    fn rollback(self: Box<Self>) {
        self.rolled_back.store(true, Ordering::SeqCst);
    }
}

struct FakeCursor;
impl ResultCursor for FakeCursor {}

fn fake_tx(db_key: &str) -> (Box<FakeTx>, Arc<AtomicBool>) {
    let flag = Arc::new(AtomicBool::new(false));
    let tx = FakeTx {
        rolled_back: flag.clone(),
        _ops: vec![BatchOp::InsertNode {
            label: "Thing".into(),
            key: db_key.into(),
            props: vec![("n".into(), Value::Int(1))],
        }],
    };
    (Box::new(tx), flag)
}

// ---------------------------------------------------------------- auth

#[test]
fn valid_token_reaches_ready() {
    let mut conn = connection(Duration::from_secs(30));
    assert_eq!(conn.state().name(), "NotAuthenticated");
    let out = conn.handle(hello("basic", FULL));
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    assert_eq!(conn.state().name(), "Ready");
    assert_eq!(conn.identity(), Some(&Identity::Full));
}

#[test]
fn role_token_resolves_to_its_role() {
    let mut conn = connection(Duration::from_secs(30));
    conn.handle(hello("bearer", ROLE));
    assert_eq!(conn.identity(), Some(&Identity::Role("analyst".into())));
}

#[test]
fn invalid_token_is_unauthorized_and_closes_the_connection() {
    let mut conn = connection(Duration::from_secs(30));
    let out = conn.handle(hello("basic", "nope"));
    assert_eq!(failure_code(&out), "Neo.ClientError.Security.Unauthorized");
    assert!(conn.is_defunct());
    assert_eq!(conn.identity(), None);
}

#[test]
fn failed_login_gives_the_same_answer_whether_or_not_the_token_exists() {
    let a = connection(Duration::from_secs(30)).handle(hello("basic", "nope"));
    let b = connection(Duration::from_secs(30)).handle(hello("basic", "full-secreT"));
    let c = connection(Duration::from_secs(30)).handle(hello("kerberos", FULL));
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn open_server_accepts_everyone_as_full_like_http() {
    let open = Arc::new(TokenAuthenticator::new(None, HashMap::new()));
    for msg in [
        hello("none", ""),
        hello("basic", "anything"),
        hello("kerberos", "x"),
    ] {
        let mut conn = Connection::new(open.clone(), Duration::from_secs(30), "c".into(), (4, 4));
        conn.handle(msg);
        assert_eq!(conn.identity(), Some(&Identity::Full));
        assert_eq!(conn.state().name(), "Ready");
    }
}

#[test]
fn token_server_refuses_scheme_none() {
    let mut conn = connection(Duration::from_secs(30));
    assert_eq!(
        failure_code(&conn.handle(hello("none", ""))),
        "Neo.ClientError.Security.Unauthorized"
    );
    assert!(conn.is_defunct());
}

// ---------------------------------------------------------------- RESET / GOODBYE

#[test]
fn reset_from_a_transaction_rolls_it_back_and_returns_to_ready() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut conn = ready();
    let (tx, rolled_back) = fake_tx("tx-node");
    conn.enter_tx(tx, Instant::now()).ok().unwrap();
    assert_eq!(conn.state().name(), "InTx");

    let out = conn.handle(ClientMessage::Reset);
    assert!(matches!(out.as_slice(), [ServerMessage::Success(m)] if m.is_empty()));
    assert_eq!(conn.state().name(), "Ready");
    assert!(rolled_back.load(Ordering::SeqCst), "buffer was dropped");
    assert_eq!(db.reader().resolve_key("tx-node"), None);
}

#[test]
fn reset_closes_a_cursor_and_clears_a_failure() {
    let mut conn = ready();
    // A failed request, then RESET.
    let out = conn.handle(other(MessageKind::Run));
    assert_eq!(failure_code(&out), "Neo.DatabaseError.General.UnknownError");
    assert_eq!(conn.state().name(), "Failed");
    let out = conn.handle(ClientMessage::Reset);
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn goodbye_rolls_back_and_closes_with_no_reply() {
    let mut conn = ready();
    let (tx, rolled_back) = fake_tx("g");
    conn.enter_tx(tx, Instant::now()).ok().unwrap();
    assert!(conn.handle(ClientMessage::Goodbye).is_empty());
    assert!(conn.is_defunct());
    assert!(rolled_back.load(Ordering::SeqCst));
    // Nothing is answered once defunct.
    assert!(conn.handle(ClientMessage::Reset).is_empty());
}

#[test]
fn goodbye_before_hello_closes() {
    let mut conn = connection(Duration::from_secs(30));
    assert!(conn.handle(ClientMessage::Goodbye).is_empty());
    assert!(conn.is_defunct());
}

// ---------------------------------------------------------------- state machine

#[test]
fn any_message_but_hello_before_hello_is_a_protocol_violation() {
    for msg in [
        ClientMessage::Reset,
        other(MessageKind::Run),
        other(MessageKind::Begin),
    ] {
        let mut conn = connection(Duration::from_secs(30));
        let out = conn.handle(msg);
        assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
        assert!(conn.is_defunct());
    }
}

#[test]
fn second_hello_is_a_protocol_violation() {
    let mut conn = ready();
    let out = conn.handle(hello("basic", FULL));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
    assert!(conn.is_defunct());
}

#[test]
fn illegal_messages_in_ready_are_protocol_violations() {
    for kind in [
        MessageKind::Pull,
        MessageKind::Discard,
        MessageKind::Commit,
        MessageKind::Rollback,
    ] {
        let mut conn = ready();
        let out = conn.handle(other(kind));
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Request.Invalid",
            "{kind:?}"
        );
        assert!(conn.is_defunct(), "{kind:?}");
    }
}

#[test]
fn legal_messages_not_built_yet_say_so() {
    for kind in [MessageKind::Run, MessageKind::Begin, MessageKind::Route] {
        let mut conn = ready();
        let out = conn.handle(other(kind));
        assert_eq!(
            failure_code(&out),
            "Neo.DatabaseError.General.UnknownError",
            "{kind:?}"
        );
    }
}

#[test]
fn failed_state_ignores_everything_but_reset_and_goodbye() {
    let mut conn = ready();
    conn.handle(other(MessageKind::Run));
    assert_eq!(conn.state().name(), "Failed");
    for msg in [
        other(MessageKind::Run),
        other(MessageKind::Begin),
        other(MessageKind::Pull),
        other(MessageKind::Commit),
    ] {
        assert_eq!(conn.handle(msg), vec![ServerMessage::Ignored]);
        assert_eq!(conn.state().name(), "Failed");
    }
    assert!(conn.handle(ClientMessage::Goodbye).is_empty());
    assert!(conn.is_defunct());
}

fn streaming() -> Connection {
    let mut conn = ready();
    conn.enter_streaming(Box::new(FakeCursor)).ok().unwrap();
    assert_eq!(conn.state().name(), "Streaming");
    conn
}

#[test]
fn streaming_allows_pull_and_discard_only() {
    for kind in [
        MessageKind::Run,
        MessageKind::Begin,
        MessageKind::Commit,
        MessageKind::Rollback,
        MessageKind::Route,
    ] {
        let out = streaming().handle(other(kind));
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Request.Invalid",
            "{kind:?}"
        );
    }
    for kind in [MessageKind::Pull, MessageKind::Discard] {
        let out = streaming().handle(other(kind));
        assert_eq!(failure_code(&out), "Neo.DatabaseError.General.UnknownError");
    }
}

#[test]
fn reset_drops_an_open_cursor() {
    let mut conn = streaming();
    conn.handle(ClientMessage::Reset);
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn in_tx_rejects_begin_and_route_and_a_second_pull_without_cursor() {
    for kind in [
        MessageKind::Begin,
        MessageKind::Route,
        MessageKind::Pull,
        MessageKind::Discard,
    ] {
        let mut conn = ready();
        let (tx, rolled_back) = fake_tx("t");
        conn.enter_tx(tx, Instant::now()).ok().unwrap();
        let out = conn.handle(other(kind));
        assert_eq!(
            failure_code(&out),
            "Neo.ClientError.Request.Invalid",
            "{kind:?}"
        );
        assert!(conn.is_defunct(), "{kind:?}");
        assert!(
            rolled_back.load(Ordering::SeqCst),
            "a failed tx is rolled back"
        );
    }
    for kind in [MessageKind::Run, MessageKind::Commit, MessageKind::Rollback] {
        let mut conn = ready();
        let (tx, _) = fake_tx("t");
        conn.enter_tx(tx, Instant::now()).ok().unwrap();
        let out = conn.handle(other(kind));
        assert_eq!(
            failure_code(&out),
            "Neo.DatabaseError.General.UnknownError",
            "{kind:?}"
        );
    }
}

#[test]
fn enter_tx_is_refused_outside_ready() {
    let mut conn = connection(Duration::from_secs(30));
    let (tx, _) = fake_tx("t");
    assert!(conn.enter_tx(tx, Instant::now()).is_err());
}

#[test]
fn interrupt_ignores_queued_requests_until_reset() {
    let mut conn = ready();
    let (tx, rolled_back) = fake_tx("t");
    conn.enter_tx(tx, Instant::now()).ok().unwrap();
    conn.interrupt();
    assert_eq!(conn.state().name(), "Interrupted");
    assert!(rolled_back.load(Ordering::SeqCst));
    assert_eq!(
        conn.handle(other(MessageKind::Run)),
        vec![ServerMessage::Ignored]
    );
    let out = conn.handle(ClientMessage::Reset);
    assert!(matches!(out.as_slice(), [ServerMessage::Success(_)]));
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn malformed_message_is_a_protocol_violation_in_every_state() {
    let mut conn = ready();
    let out = conn.handle_malformed(BoltError::Invalid("bad".into()));
    assert_eq!(failure_code(&out), "Neo.ClientError.Request.Invalid");
    assert!(conn.is_defunct());
    // Also from Failed, where requests are otherwise ignored.
    let mut failed = ready();
    failed.handle(other(MessageKind::Run));
    assert_eq!(failed.state().name(), "Failed");
    failed.handle_malformed(BoltError::Invalid("bad".into()));
    assert!(failed.is_defunct());
    // And inside a transaction, which it rolls back.
    let mut in_tx = ready();
    let (tx, rolled_back) = fake_tx("t");
    in_tx.enter_tx(tx, Instant::now()).ok().unwrap();
    in_tx.handle_malformed(BoltError::Invalid("bad".into()));
    assert!(in_tx.is_defunct());
    assert!(rolled_back.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------- tx timeout

#[test]
fn idle_transaction_is_rolled_back_and_the_next_request_learns_why() {
    let start = Instant::now();
    let mut conn = connection(Duration::from_secs(30));
    conn.handle_at(hello("basic", FULL), start);
    let (tx, rolled_back) = fake_tx("t");
    conn.enter_tx(tx, start).ok().unwrap();

    assert!(!conn.check_timeout(start + Duration::from_secs(29)));
    assert!(!rolled_back.load(Ordering::SeqCst));
    assert!(conn.check_timeout(start + Duration::from_secs(30)));
    assert!(rolled_back.load(Ordering::SeqCst));
    assert_eq!(conn.state().name(), "Failed");

    let out = conn.handle_at(other(MessageKind::Commit), start + Duration::from_secs(31));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Transaction.TransactionTimedOut"
    );
    // Once told, later requests are ignored until RESET.
    assert_eq!(
        conn.handle_at(other(MessageKind::Run), start + Duration::from_secs(32)),
        vec![ServerMessage::Ignored]
    );
    conn.handle_at(ClientMessage::Reset, start + Duration::from_secs(33));
    assert_eq!(conn.state().name(), "Ready");
}

#[test]
fn timeout_check_on_message_receipt_expires_without_the_ticker() {
    let start = Instant::now();
    let mut conn = connection(Duration::from_secs(5));
    conn.handle_at(hello("basic", FULL), start);
    let (tx, rolled_back) = fake_tx("t");
    conn.enter_tx(tx, start).ok().unwrap();
    let out = conn.handle_at(other(MessageKind::Run), start + Duration::from_secs(6));
    assert_eq!(
        failure_code(&out),
        "Neo.ClientError.Transaction.TransactionTimedOut"
    );
    assert!(rolled_back.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------- the read loop

const OFFER: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04,
    0x00, 0x00, 0x00, 0x03,
];

fn framed(msg: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    framing::frame(msg, &mut buf);
    buf
}

/// Reads one framed response and returns its tag.
async fn read_tag<R: AsyncReadExt + Unpin>(stream: &mut R) -> u8 {
    let mut stream_bytes = Vec::new();
    loop {
        match framing::deframe(&stream_bytes) {
            Ok((msg, _)) => {
                let (value, _) = packstream::unpack(&msg).unwrap();
                let PackValue::Struct(s) = value else {
                    panic!("response is not a structure")
                };
                return s.tag;
            }
            Err(_) => {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).await.unwrap();
                stream_bytes.push(byte[0]);
            }
        }
    }
}

async fn client_and_task() -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let task = tokio::spawn(serve_connection(
        server,
        authenticator(),
        "bolt-test".into(),
        ConnectionConfig::default(),
    ));
    (client, task)
}

async fn handshake_and_hello(client: &mut tokio::io::DuplexStream) {
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [0, 0, 0, 5]);
    client
        .write_all(&framed(&hello_bytes("basic", FULL)))
        .await
        .unwrap();
    assert_eq!(read_tag(client).await, TAG_SUCCESS);
}

#[tokio::test]
async fn wire_hello_then_goodbye_closes_cleanly() {
    let (mut client, task) = client_and_task().await;
    handshake_and_hello(&mut client).await;
    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_bad_token_gets_a_failure_then_the_socket_closes() {
    let (mut client, task) = client_and_task().await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    client
        .write_all(&framed(&hello_bytes("basic", "wrong")))
        .await
        .unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn wire_noops_between_and_after_messages_are_keepalives() {
    let (mut client, task) = client_and_task().await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();

    // NOOPs before HELLO, then HELLO, then NOOPs only, then RESET.
    let mut bytes = vec![0u8; 2000];
    bytes.extend(framed(&hello_bytes("basic", FULL)));
    client.write_all(&bytes).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);

    client.write_all(&vec![0u8; 4096]).await.unwrap();
    client.write_all(&[0, 0]).await.unwrap();
    client
        .write_all(&framed(&message_bytes(TAG_RESET, vec![])))
        .await
        .unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);

    // A message arriving one byte at a time, with NOOPs in front of it.
    let mut piecewise = vec![0u8, 0, 0, 0];
    piecewise.extend(framed(&message_bytes(TAG_RESET, vec![])));
    for byte in piecewise {
        client.write_all(&[byte]).await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);

    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn wire_idle_client_is_not_spun_on_and_is_still_served_later() {
    let (mut client, task) = client_and_task().await;
    handshake_and_hello(&mut client).await;
    // A long idle stretch, with nothing but a lone NOOP in the buffer.
    client.write_all(&[0, 0]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(!task.is_finished(), "an idle connection stays open");
    client
        .write_all(&framed(&message_bytes(TAG_RESET, vec![])))
        .await
        .unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);
    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_reset_queued_behind_requests_ignores_them() {
    let (mut client, task) = client_and_task().await;
    handshake_and_hello(&mut client).await;
    // RUN, PULL and RESET in one write: the first two are IGNORED.
    let mut batch = Vec::new();
    batch.extend(framed(&message_bytes(TAG_RUN, vec![])));
    batch.extend(framed(&message_bytes(TAG_PULL, vec![])));
    batch.extend(framed(&message_bytes(TAG_RESET, vec![])));
    client.write_all(&batch).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_IGNORED);
    assert_eq!(read_tag(&mut client).await, TAG_IGNORED);
    assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);
    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_http_on_the_bolt_port_is_dropped_without_a_reply() {
    let (mut client, task) = client_and_task().await;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    task.await.unwrap().unwrap();
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn wire_unmatched_version_gets_zeros_and_close() {
    let (mut client, task) = client_and_task().await;
    let mut offer = OFFER;
    offer[8..20].fill(0);
    offer[19] = 3;
    client.write_all(&offer).await.unwrap();
    let mut reply = [9u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [0; 4]);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_malformed_message_is_a_violation_and_closes() {
    let (mut client, task) = client_and_task().await;
    handshake_and_hello(&mut client).await;
    // 0xC1 is a float marker with no payload: truncated PackStream.
    client.write_all(&framed(&[0xC1])).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn wire_unknown_signature_is_a_violation_and_closes() {
    let (mut client, task) = client_and_task().await;
    handshake_and_hello(&mut client).await;
    client
        .write_all(&framed(&message_bytes(0x55, vec![])))
        .await
        .unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
}

async fn spawn_with(
    config: ConnectionConfig,
    buffer: usize,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let (client, server) = tokio::io::duplex(buffer);
    let task = tokio::spawn(serve_connection(
        server,
        authenticator(),
        "b".into(),
        config,
    ));
    (client, task)
}

fn expect_timed_out(result: std::io::Result<()>) {
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn wire_oversized_request_after_hello_is_refused_and_closed() {
    let config = ConnectionConfig {
        max_message_bytes: 1024,
        ..ConnectionConfig::default()
    };
    let (mut client, task) = spawn_with(config, 1 << 20).await;
    handshake_and_hello(&mut client).await;
    // The first chunk header promises 65535 bytes that never complete.
    let mut partial = vec![0xFF, 0xFF];
    partial.extend(vec![0u8; 4000]);
    client.write_all(&partial).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_pre_auth_cap_is_small_and_default_is_64_kib() {
    assert_eq!(
        ConnectionConfig::default().pre_auth_max_message_bytes,
        64 * 1024
    );
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 20).await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    // A full chunk, then more of the same: over 64 KiB with no HELLO.
    let mut flood = Vec::new();
    for _ in 0..2 {
        flood.extend([0xFF, 0xFF]);
        flood.extend(vec![0u8; 65535]);
    }
    client.write_all(&flood).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn wire_pre_auth_cap_does_not_apply_after_hello() {
    // A 100 KiB request after HELLO is within the post-auth cap. It is an
    // unknown tag, so the answer is a violation FAILURE, not a size refusal.
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 20).await;
    handshake_and_hello(&mut client).await;
    let big = PackValue::Struct(PackStruct::new(
        0x55,
        vec![PackValue::Bytes(vec![0; 100_000])],
    ));
    let mut bytes = Vec::new();
    packstream::pack(&big, &mut bytes);
    client.write_all(&framed(&bytes)).await.unwrap();
    assert_eq!(read_tag(&mut client).await, TAG_FAILURE);
    task.await.unwrap().unwrap();
}

// ---------------------------------------------------------------- deadlines

#[tokio::test(start_paused = true)]
async fn wire_no_handshake_times_out() {
    let (_client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_no_hello_times_out_after_the_handshake() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_pre_hello_keepalives_do_not_hold_the_connection() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    for _ in 0..20 {
        if client.write_all(&[0, 0]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_hello_dripped_one_byte_at_a_time_times_out() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    let hello = framed(&hello_bytes("basic", FULL));
    for byte in hello {
        if client.write_all(&[byte]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_stalled_mid_message_times_out_after_hello() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    handshake_and_hello(&mut client).await;
    // A chunk header and a few bytes, then silence.
    client.write_all(&[0x00, 0x20, 0xB0, 0x0F]).await.unwrap();
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_dripping_a_message_after_hello_times_out() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    handshake_and_hello(&mut client).await;
    client.write_all(&[0x00, 0xFF]).await.unwrap();
    for _ in 0..200 {
        if client.write_all(&[0]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    expect_timed_out(task.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn wire_noop_only_client_after_hello_stays_connected() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 16).await;
    handshake_and_hello(&mut client).await;
    for _ in 0..120 {
        client.write_all(&[0, 0]).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(!task.is_finished());
    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn wire_a_client_that_never_reads_hits_the_write_timeout() {
    // A 16-byte pipe: the HELLO reply cannot be written while nobody reads.
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 16).await;
    client.write_all(&OFFER).await.unwrap();
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await.unwrap();
    client
        .write_all(&framed(&hello_bytes("basic", FULL)))
        .await
        .unwrap();
    expect_timed_out(task.await.unwrap());
}

#[tokio::test]
async fn wire_long_pipeline_is_handled_in_order() {
    let (mut client, task) = spawn_with(ConnectionConfig::default(), 1 << 20).await;
    handshake_and_hello(&mut client).await;
    let mut batch = Vec::new();
    for _ in 0..5000 {
        batch.extend(framed(&message_bytes(TAG_RESET, vec![])));
    }
    client.write_all(&batch).await.unwrap();
    // The RESETs interrupt then reset; every one is answered SUCCESS.
    for _ in 0..5000 {
        assert_eq!(read_tag(&mut client).await, TAG_SUCCESS);
    }
    client
        .write_all(&framed(&message_bytes(TAG_GOODBYE, vec![])))
        .await
        .unwrap();
    task.await.unwrap().unwrap();
}
