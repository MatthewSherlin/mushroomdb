//! `mushroomdb serve --bolt-port` (TX-16): the Bolt listener beside the HTTP
//! server, its startup refusals, and its part in the graceful shutdown.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bolt::framing;
use bolt::messages::{
    TAG_BEGIN, TAG_FAILURE, TAG_GOODBYE, TAG_HELLO, TAG_PULL, TAG_RECORD, TAG_RUN, TAG_SUCCESS,
};
use bolt::packstream::{self, PackStruct, PackValue};

const TOKEN: &str = "bolt-cli-test-token";

/// The handshake every current driver sends.
const OFFER: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04,
    0x00, 0x00, 0x00, 0x03,
];

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-bolt-{name}-{}-{nanos}", std::process::id()))
}

/// `mushroomdb serve <dir> --no-ui <args>`, with no Bolt settings inherited.
fn serve(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("serve").arg(dir).arg("--no-ui").args(args);
    for var in [
        "MUSHROOMDB_BOLT_PORT",
        "MUSHROOMDB_BOLT_TLS",
        "MUSHROOMDB_BOLT_ADVERTISED_HOST",
        "MUSHROOMDB_BOLT_ADVERTISED_PORT",
        "MUSHROOMDB_BOLT_MAX_CONNECTIONS",
        "MUSHROOMDB_TOKEN",
        "MUSHROOMDB_BACKUP_S3_URL",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

struct Server {
    child: Child,
    bolt: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts serve with a Bolt listener on a free loopback port and returns once
/// both listeners are up.
fn start(dir: &Path, extra: &[&str]) -> Server {
    let mut args = vec![
        "--addr",
        "127.0.0.1:0",
        "--token",
        TOKEN,
        "--bolt-port",
        "0",
    ];
    args.extend_from_slice(extra);
    let child = serve(dir, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    let mut server = Server {
        child,
        bolt: String::new(),
    };
    let mut lines = BufReader::new(server.child.stdout.take().unwrap()).lines();
    match lines.next() {
        Some(Ok(line)) if line.starts_with("listening on http://") => {}
        other => panic!("serve did not start: {other:?}"),
    }
    match lines.next() {
        Some(Ok(line)) if line.starts_with("listening on bolt://") => {
            server.bolt = line["listening on bolt://".len()..].trim().to_string();
        }
        other => panic!("no Bolt listener was reported: {other:?}"),
    }
    // Keep draining so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    server
}

/// A blocking Bolt client.
struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn connect(addr: &str) -> Self {
        let stream = TcpStream::connect(addr).expect("connect to the Bolt port");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut client = Self {
            stream,
            buf: Vec::new(),
        };
        client.stream.write_all(&OFFER).unwrap();
        let mut reply = [0u8; 4];
        client.stream.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [0, 0, 0, 5], "Bolt 5.0 is negotiated");
        client
    }

    fn send(&mut self, tag: u8, fields: Vec<PackValue>) {
        let mut message = Vec::new();
        packstream::pack(
            &PackValue::Struct(PackStruct::new(tag, fields)),
            &mut message,
        );
        let mut framed = Vec::new();
        framing::frame(&message, &mut framed);
        self.stream.write_all(&framed).unwrap();
    }

    fn recv(&mut self) -> Option<PackStruct> {
        loop {
            if let Ok((message, used)) = framing::deframe(&self.buf) {
                self.buf.drain(..used);
                let (value, _) = packstream::unpack(&message).unwrap();
                let PackValue::Struct(response) = value else {
                    panic!("a response is a structure")
                };
                return Some(response);
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn hello(&mut self, token: &str) -> PackStruct {
        let extra = PackValue::Map(vec![
            ("scheme".into(), PackValue::String("bearer".into())),
            ("credentials".into(), PackValue::String(token.into())),
        ]);
        self.send(TAG_HELLO, vec![extra]);
        self.recv().expect("a HELLO reply")
    }

    fn run(&mut self, query: &str) -> PackStruct {
        self.send(
            TAG_RUN,
            vec![
                PackValue::String(query.into()),
                PackValue::Map(vec![]),
                PackValue::Map(vec![]),
            ],
        );
        self.recv().expect("a RUN reply")
    }

    fn pull_all(&mut self) -> (Vec<PackStruct>, PackStruct) {
        self.send(
            TAG_PULL,
            vec![PackValue::Map(vec![("n".into(), PackValue::Int(-1))])],
        );
        let mut records = Vec::new();
        loop {
            let reply = self.recv().expect("a PULL reply");
            if reply.tag == TAG_RECORD {
                records.push(reply);
            } else {
                return (records, reply);
            }
        }
    }

    /// Whether the server has closed the socket (reads end, or fail).
    fn closed(&mut self) -> bool {
        self.recv().is_none()
    }
}

#[test]
fn serve_answers_hello_run_pull_goodbye_on_the_bolt_port() {
    let dir = tmp("cycle");
    let server = start(&dir, &[]);
    let mut client = Client::connect(&server.bolt);
    assert_eq!(client.hello(TOKEN).tag, TAG_SUCCESS);
    assert_eq!(client.run("CREATE (n:Thing {id: 'b1'})").tag, TAG_SUCCESS);
    assert_eq!(client.pull_all().1.tag, TAG_SUCCESS);
    assert_eq!(client.run("MATCH (n:Thing) RETURN n.id").tag, TAG_SUCCESS);
    let (records, summary) = client.pull_all();
    assert_eq!(summary.tag, TAG_SUCCESS);
    assert_eq!(
        records[0].fields,
        vec![PackValue::List(vec![PackValue::String("b1".into())])]
    );
    client.send(TAG_GOODBYE, vec![]);
    assert!(client.closed());

    // The HTTP token is the Bolt credential; anything else is refused.
    let mut stranger = Client::connect(&server.bolt);
    assert_eq!(stranger.hello("wrong-token").tag, TAG_FAILURE);
    assert!(stranger.closed());
}

/// The Bolt listener binds `--addr`'s IP, so a non-loopback address with no
/// authentication is refused before anything listens, as it is for HTTP.
#[test]
fn a_non_loopback_bolt_listener_without_authentication_is_refused() {
    let dir = tmp("open");
    let out = serve(
        &dir,
        &[
            "--addr",
            "0.0.0.0:0",
            "--bolt-port",
            "0",
            "--bolt-advertised-host",
            "localhost",
        ],
    )
    .output()
    .expect("run serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("non-loopback"), "{stderr}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("listening on"));
}

#[test]
fn a_wildcard_address_without_an_advertised_host_is_refused() {
    let dir = tmp("wildcard");
    let out = serve(
        &dir,
        &["--addr", "0.0.0.0:0", "--token", TOKEN, "--bolt-port", "0"],
    )
    .output()
    .expect("run serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--bolt-advertised-host"), "{stderr}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("listening on"));
    // Refused before the store was created.
    assert!(!dir.join("wal.bin").exists());
}

#[test]
fn bolt_tls_without_a_certificate_is_refused() {
    let dir = tmp("no-cert");
    let out = serve(
        &dir,
        &["--addr", "127.0.0.1:0", "--bolt-port", "0", "--bolt-tls"],
    )
    .output()
    .expect("run serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--tls-cert"), "{stderr}");
}

#[test]
fn a_bolt_flag_without_a_port_is_refused() {
    let dir = tmp("no-port");
    let out = serve(&dir, &["--addr", "127.0.0.1:0", "--bolt-plaintext"])
        .output()
        .expect("run serve");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--bolt-port"), "{stderr}");
}

fn send_term(child: &Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success());
}

#[test]
fn sigterm_rolls_back_an_open_bolt_transaction_and_exits_within_the_timeout() {
    let dir = tmp("drain");
    let mut server = start(&dir, &["--shutdown-timeout-secs", "10"]);
    let mut committed = Client::connect(&server.bolt);
    committed.hello(TOKEN);
    assert_eq!(
        committed.run("CREATE (n:Thing {id: 'kept'})").tag,
        TAG_SUCCESS
    );
    assert_eq!(committed.pull_all().1.tag, TAG_SUCCESS);

    let mut open = Client::connect(&server.bolt);
    open.hello(TOKEN);
    open.send(TAG_BEGIN, vec![PackValue::Map(vec![])]);
    assert_eq!(open.recv().unwrap().tag, TAG_SUCCESS);
    assert_eq!(
        open.run("CREATE (n:Thing {id: 'in-flight'})").tag,
        TAG_SUCCESS
    );
    assert_eq!(open.pull_all().1.tag, TAG_SUCCESS);

    let started = Instant::now();
    send_term(&server.child);
    // The connection is closed by the server, well inside the drain bound.
    assert!(open.closed());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    loop {
        if server.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "serve was still running {:?} after the signal",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let reopened = mushroomdb::GraphDb::open(&dir).unwrap();
    assert!(reopened.has_node("kept"));
    assert!(
        !reopened.has_node("in-flight"),
        "the open transaction was rolled back"
    );
}
