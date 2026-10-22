//! The Bolt TCP listener over real sockets: plaintext and TLS, the full
//! request cycle, key tiers, the handshake deadline, the connection limit, and
//! shutdown with a transaction open.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bolt::connection::ConnectionConfig;
use bolt::framing;
use bolt::messages::hello::TokenAuthenticator;
use bolt::messages::route::RoutingConfig;
use bolt::messages::{
    TAG_BEGIN, TAG_FAILURE, TAG_GOODBYE, TAG_HELLO, TAG_PULL, TAG_RECORD, TAG_ROUTE, TAG_RUN,
    TAG_SUCCESS,
};
use bolt::packstream::{self, PackStruct, PackValue};
use bolt::server::{BoltConfig, BoltListener};
use mushroomdb::{AccessKeys, RoleDef, Schema, SharedDb};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

const TOKEN: &str = "full-secret";
const READ_KEY: &str = "rd-key-0123456789abcdef0123456789abcdef";

/// The handshake every current driver sends.
const OFFER: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04,
    0x00, 0x00, 0x00, 0x03,
];

struct Running {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    db: SharedDb,
    _dir: tempfile::TempDir,
}

impl Running {
    /// Gives the stop signal and waits for `serve` to return. Hands back the
    /// store directory, which is deleted when it is dropped.
    async fn stop(self, within: Duration) -> (Duration, tempfile::TempDir) {
        let started = Instant::now();
        self.stop.send(true).unwrap();
        tokio::time::timeout(within, self.task)
            .await
            .expect("serve returns within the bound")
            .unwrap()
            .unwrap();
        (started.elapsed(), self._dir)
    }
}

fn authenticator(keys: Option<AccessKeys>) -> Arc<TokenAuthenticator> {
    let auth = TokenAuthenticator::new(Some(TOKEN.into()), HashMap::new());
    Arc::new(match keys {
        Some(keys) => auth.with_access_keys(keys),
        None => auth,
    })
}

/// Starts a listener on a free loopback port; `tune` adjusts the config once
/// the bound address is known.
async fn start(
    keys: Option<AccessKeys>,
    tune: impl FnOnce(&mut BoltConfig, SocketAddr),
) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    start_on(db, dir, keys, tune).await
}

async fn start_on(
    db: SharedDb,
    dir: tempfile::TempDir,
    keys: Option<AccessKeys>,
    tune: impl FnOnce(&mut BoltConfig, SocketAddr),
) -> Running {
    let listener = BoltListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = BoltConfig {
        connection: ConnectionConfig {
            routing: Some(RoutingConfig::new(addr, None, "mushroomdb").unwrap()),
            ..ConnectionConfig::default()
        },
        ..BoltConfig::default()
    };
    tune(&mut config, addr);
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(listener.serve(db.clone(), authenticator(keys), config, stop_rx));
    Running {
        addr,
        stop,
        task,
        db,
        _dir: dir,
    }
}

/// A minimal Bolt client over any stream.
struct Client<S> {
    stream: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    async fn handshake(&mut self) -> [u8; 4] {
        self.stream.write_all(&OFFER).await.unwrap();
        let mut reply = [0u8; 4];
        self.stream.read_exact(&mut reply).await.unwrap();
        reply
    }

    async fn send(&mut self, tag: u8, fields: Vec<PackValue>) {
        let mut message = Vec::new();
        packstream::pack(
            &PackValue::Struct(PackStruct::new(tag, fields)),
            &mut message,
        );
        let mut framed = Vec::new();
        framing::frame(&message, &mut framed);
        self.stream.write_all(&framed).await.unwrap();
    }

    /// The next response, or `None` once the server has closed.
    async fn recv(&mut self) -> Option<PackStruct> {
        loop {
            if let Ok((message, used)) = framing::deframe(&self.buf) {
                self.buf.drain(..used);
                let (value, _) = packstream::unpack(&message).unwrap();
                let PackValue::Struct(response) = value else {
                    panic!("a response is a structure")
                };
                return Some(response);
            }
            let read =
                tokio::time::timeout(Duration::from_secs(10), self.stream.read_buf(&mut self.buf))
                    .await
                    .expect("the server answers or closes");
            match read {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn hello(&mut self, token: &str) -> PackStruct {
        let extra = PackValue::Map(vec![
            (
                "user_agent".into(),
                PackValue::String("listener-test/1".into()),
            ),
            ("scheme".into(), PackValue::String("basic".into())),
            ("principal".into(), PackValue::String("anyone".into())),
            ("credentials".into(), PackValue::String(token.into())),
        ]);
        self.send(TAG_HELLO, vec![extra]).await;
        self.recv().await.expect("a HELLO reply")
    }

    async fn run(&mut self, query: &str) -> PackStruct {
        self.send(
            TAG_RUN,
            vec![
                PackValue::String(query.into()),
                PackValue::Map(vec![]),
                PackValue::Map(vec![]),
            ],
        )
        .await;
        self.recv().await.expect("a RUN reply")
    }

    /// `PULL` everything: the records, then the closing reply.
    async fn pull_all(&mut self) -> (Vec<Vec<PackValue>>, PackStruct) {
        self.send(
            TAG_PULL,
            vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
        )
        .await;
        let mut records = Vec::new();
        loop {
            let reply = self.recv().await.expect("a PULL reply");
            if reply.tag == TAG_RECORD {
                let [PackValue::List(row)] = reply.fields.as_slice() else {
                    panic!("a record carries one list")
                };
                records.push(row.clone());
            } else {
                return (records, reply);
            }
        }
    }

    /// Whether the server closes the socket within `within`.
    async fn closed_within(&mut self, within: Duration) -> bool {
        let mut rest = Vec::new();
        tokio::time::timeout(within, self.stream.read_to_end(&mut rest))
            .await
            .is_ok()
    }
}

async fn connect(addr: SocketAddr) -> Client<TcpStream> {
    Client::new(TcpStream::connect(addr).await.unwrap())
}

/// The `code` of a `FAILURE`.
fn failure_code(reply: &PackStruct) -> String {
    assert_eq!(reply.tag, TAG_FAILURE, "{reply:?}");
    let [PackValue::Map(meta)] = reply.fields.as_slice() else {
        panic!("a FAILURE carries one map")
    };
    match meta.iter().find(|(k, _)| k == "code") {
        Some((_, PackValue::String(code))) => code.clone(),
        other => panic!("no code: {other:?}"),
    }
}

/// HELLO, RUN, PULL, GOODBYE: the whole cycle on an established stream.
async fn full_cycle<S: AsyncRead + AsyncWrite + Unpin>(client: &mut Client<S>) {
    assert_eq!(client.handshake().await, [0, 0, 0, 5]);
    assert_eq!(client.hello(TOKEN).await.tag, TAG_SUCCESS);
    let run = client.run("RETURN 1 + 1 AS two").await;
    assert_eq!(run.tag, TAG_SUCCESS, "{run:?}");
    let (records, summary) = client.pull_all().await;
    assert_eq!(records, vec![vec![PackValue::Int(2)]]);
    assert_eq!(summary.tag, TAG_SUCCESS);
    client.send(TAG_GOODBYE, vec![]).await;
    assert!(client.closed_within(Duration::from_secs(5)).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_handshake_negotiates_a_version() {
    let server = start(None, |_, _| {}).await;
    let mut client = connect(server.addr).await;
    assert_eq!(client.handshake().await, [0, 0, 0, 5]);
    server.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_run_pull_goodbye_over_a_real_socket() {
    let server = start(None, |_, _| {}).await;
    full_cycle(&mut connect(server.addr).await).await;
    // A write lands in the store the listener was given.
    let mut client = connect(server.addr).await;
    client.handshake().await;
    client.hello(TOKEN).await;
    assert_eq!(
        client.run("CREATE (n:Thing {id: 'w1'})").await.tag,
        TAG_SUCCESS
    );
    assert_eq!(client.pull_all().await.1.tag, TAG_SUCCESS);
    assert!(server.db.read().has_node("w1"));
    server.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_advertises_the_configured_address() {
    let server = start(None, |config, addr| {
        config.connection.routing = Some(
            RoutingConfig::new(addr, Some("db.example.test"), "Orders")
                .unwrap()
                .with_advertised_port(17687),
        );
    })
    .await;
    let mut client = connect(server.addr).await;
    client.handshake().await;
    client.hello(TOKEN).await;
    client
        .send(
            TAG_ROUTE,
            vec![
                PackValue::Map(vec![]),
                PackValue::List(vec![]),
                PackValue::Map(vec![("db".into(), PackValue::String("ORDERS".into()))]),
            ],
        )
        .await;
    let reply = client.recv().await.unwrap();
    assert_eq!(reply.tag, TAG_SUCCESS, "{reply:?}");
    let text = format!("{reply:?}");
    assert!(text.contains("db.example.test:17687"), "{text}");
    // RUN accepts the store's name, in any case, as ROUTE does.
    client
        .send(
            TAG_RUN,
            vec![
                PackValue::String("RETURN 1".into()),
                PackValue::Map(vec![]),
                PackValue::Map(vec![("db".into(), PackValue::String("orders".into()))]),
            ],
        )
        .await;
    assert_eq!(client.recv().await.unwrap().tag, TAG_SUCCESS);
    server.stop(Duration::from_secs(5)).await;
}

/// A store with a `viewer` role and a key file holding a `read` key for it.
fn viewer_store() -> (SharedDb, tempfile::TempDir, AccessKeys) {
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
    let keys = AccessKeys::from_json(&format!(
        r#"{{"keys":[{{"hash":"{}","tier":"read","role":"viewer"}}]}}"#,
        mushroomdb::hash_token(READ_KEY).unwrap()
    ))
    .unwrap();
    (db, dir, keys)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_key_logs_in_and_reads_but_cannot_write() {
    let (db, dir, keys) = viewer_store();
    let server = start_on(db, dir, Some(keys), |_, _| {}).await;
    let mut client = connect(server.addr).await;
    client.handshake().await;
    assert_eq!(client.hello(READ_KEY).await.tag, TAG_SUCCESS);
    assert_eq!(
        client.run("MATCH (n:Doc) RETURN count(n)").await.tag,
        TAG_SUCCESS
    );
    assert_eq!(client.pull_all().await.1.tag, TAG_SUCCESS);
    let refused = client.run("CREATE (n:Doc {id: 'nope'})").await;
    assert_eq!(failure_code(&refused), "Neo.ClientError.Security.Forbidden");
    assert!(!server.db.read().has_node("nope"));
    // A wrong token is refused and the socket closed.
    let mut stranger = connect(server.addr).await;
    stranger.handshake().await;
    assert_eq!(
        failure_code(&stranger.hello("not-a-key").await),
        "Neo.ClientError.Security.Unauthorized"
    );
    assert!(stranger.closed_within(Duration::from_secs(5)).await);
    server.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_sends_nothing_is_dropped_at_the_handshake_deadline() {
    let server = start(None, |config, _| {
        config.connection.hello_timeout = Duration::from_millis(300);
    })
    .await;
    let mut silent = connect(server.addr).await;
    let started = Instant::now();
    assert!(silent.closed_within(Duration::from_secs(5)).await);
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    // The listener still serves everyone else.
    full_cycle(&mut connect(server.addr).await).await;
    server.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_over_the_connection_limit_is_refused_with_a_transient_failure() {
    let server = start(None, |config, _| config.max_connections = 1).await;
    let mut first = connect(server.addr).await;
    first.handshake().await;
    assert_eq!(first.hello(TOKEN).await.tag, TAG_SUCCESS);

    let mut second = connect(server.addr).await;
    assert_eq!(second.handshake().await, [0, 0, 0, 5]);
    let refused = second.hello(TOKEN).await;
    assert_eq!(
        failure_code(&refused),
        "Neo.TransientError.General.DatabaseUnavailable"
    );
    assert!(format!("{refused:?}").contains("limit of 1"));
    assert!(second.closed_within(Duration::from_secs(5)).await);

    // Once the first leaves, its slot is free again.
    first.send(TAG_GOODBYE, vec![]).await;
    assert!(first.closed_within(Duration::from_secs(5)).await);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut next = connect(server.addr).await;
        next.handshake().await;
        if next.hello(TOKEN).await.tag == TAG_SUCCESS {
            break;
        }
        assert!(Instant::now() < deadline, "the slot was never released");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_rolls_back_an_open_transaction_and_stops_accepting() {
    let server = start(None, |config, _| {
        config.drain_timeout = Duration::from_secs(2);
    })
    .await;
    // Committed before the shutdown: stays.
    let mut committed = connect(server.addr).await;
    committed.handshake().await;
    committed.hello(TOKEN).await;
    committed.run("CREATE (n:Thing {id: 'kept'})").await;
    committed.pull_all().await;

    let mut open = connect(server.addr).await;
    open.handshake().await;
    open.hello(TOKEN).await;
    open.send(TAG_BEGIN, vec![PackValue::Map(vec![])]).await;
    assert_eq!(open.recv().await.unwrap().tag, TAG_SUCCESS);
    assert_eq!(
        open.run("CREATE (n:Thing {id: 'in-flight'})").await.tag,
        TAG_SUCCESS
    );
    assert_eq!(open.pull_all().await.1.tag, TAG_SUCCESS);

    let addr = server.addr;
    let db = server.db.clone();
    let (took, dir) = server.stop(Duration::from_secs(5)).await;
    assert!(
        took < Duration::from_secs(2),
        "closed before the drain bound: {took:?}"
    );
    // Both connections were closed by the server.
    assert!(open.closed_within(Duration::from_secs(2)).await);
    assert!(committed.closed_within(Duration::from_secs(2)).await);
    // The open transaction was rolled back; the committed write stays.
    assert!(db.read().has_node("kept"));
    assert!(!db.read().has_node("in-flight"));
    drop(db);
    let reopened = mushroomdb::GraphDb::open(dir.path()).unwrap();
    assert!(reopened.has_node("kept"));
    assert!(!reopened.has_node("in-flight"));
    // Nothing accepts on the port any more.
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_stuck_writing_is_closed_at_the_drain_bound() {
    let server = start(None, |config, _| {
        config.drain_timeout = Duration::from_millis(500);
    })
    .await;
    let mut stuck = connect(server.addr).await;
    stuck.handshake().await;
    stuck.hello(TOKEN).await;
    // A result far larger than the socket buffers, which the client never
    // reads: the server blocks writing it.
    let padding = "x".repeat(200);
    stuck
        .send(
            TAG_RUN,
            vec![
                PackValue::String(format!(
                    "UNWIND range(1, 50000) AS i RETURN i, '{padding}' AS pad"
                )),
                PackValue::Map(vec![]),
                PackValue::Map(vec![]),
            ],
        )
        .await;
    stuck
        .send(
            TAG_PULL,
            vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
        )
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (took, _dir) = server.stop(Duration::from_secs(5)).await;
    assert!(took >= Duration::from_millis(400), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[cfg(feature = "tls")]
mod tls {
    use super::*;
    use tokio_rustls::rustls;

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_full_cycle_runs_over_tls_and_plaintext_is_not_served() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        let server = start(None, |config, _| {
            config.tls = Some(Arc::new(server_config));
            config.connection.hello_timeout = Duration::from_secs(2);
        })
        .await;

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let tcp = TcpStream::connect(server.addr).await.unwrap();
        let tls = connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap();
        full_cycle(&mut Client::new(tls)).await;

        // A plaintext handshake on the TLS port gets no Bolt version back.
        let mut plain = connect(server.addr).await;
        plain.stream.write_all(&OFFER).await.unwrap();
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), plain.stream.read_to_end(&mut reply))
            .await
            .expect("the server closes a plaintext client");
        assert_ne!(reply.get(..4), Some(&[0u8, 0, 0, 5][..]), "{reply:?}");
        server.stop(Duration::from_secs(5)).await;
    }
}
