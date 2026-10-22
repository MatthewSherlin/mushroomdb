//! `mushroomdb serve` starts continuous backup when `MUSHROOMDB_BACKUP_S3_URL`
//! is set, and refuses to start when it is set wrong.
//!
//! The upload test needs a real S3-compatible service and runs only when
//! `MUSHROOMDB_TEST_S3_ENDPOINT` is set (bucket `MUSHROOMDB_TEST_S3_BUCKET`,
//! default `mushroomdb-test`, must exist; credentials default to MinIO's).
use backup::object_store::path::Path as ObjectPath;
use backup::object_store::ObjectStore as _;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-servebackup-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn serve(dir: &PathBuf, env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mushroomdb"));
    cmd.arg("serve")
        .arg(dir)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--no-ui")
        .env_remove("MUSHROOMDB_BACKUP_S3_URL");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

#[test]
fn a_malformed_backup_url_refuses_to_serve() {
    let dir = tmp("malformed");
    let out = serve(&dir, &[("MUSHROOMDB_BACKUP_S3_URL", "gs://bucket/prefix")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run mushroomdb serve");
    assert!(
        !out.status.success(),
        "serve must not start without the backup it was asked for"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("MUSHROOMDB_BACKUP_S3_URL"),
        "the error names the setting: {stderr}"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("listening on"));
}

#[test]
fn serve_ships_its_wal_to_s3() {
    let Ok(endpoint) = std::env::var("MUSHROOMDB_TEST_S3_ENDPOINT") else {
        eprintln!("MUSHROOMDB_TEST_S3_ENDPOINT is not set: skipping the S3 test");
        return;
    };
    let bucket =
        std::env::var("MUSHROOMDB_TEST_S3_BUCKET").unwrap_or_else(|_| "mushroomdb-test".into());
    let key = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
    let dir = tmp("ships");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let url = format!("minio://{bucket}/{prefix}");
    let mut child = serve(
        &dir,
        &[
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "100"),
            ("AWS_ENDPOINT_URL", endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", key.as_str()),
            ("AWS_SECRET_ACCESS_KEY", secret.as_str()),
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn mushroomdb serve");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    match lines.next() {
        Some(Ok(l)) if l.starts_with("listening on") => {}
        other => panic!("serve did not start: {other:?}"),
    }

    let lookup = |k: &str| match k {
        "AWS_ENDPOINT_URL" => Some(endpoint.clone()),
        "AWS_ACCESS_KEY_ID" => Some(key.clone()),
        "AWS_SECRET_ACCESS_KEY" => Some(secret.clone()),
        _ => None,
    };
    let store =
        backup::store::build_store_with(&backup::BackupConfig::new(url.clone()), lookup).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let latest = rt.block_on(async {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(r) = store
                .get(&ObjectPath::from(format!("{prefix}/LATEST")))
                .await
            {
                break r.bytes().await.unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no LATEST in the bucket"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let _ = child.kill();
    let _ = child.wait();
    let manifest_key = String::from_utf8(latest.to_vec()).unwrap();
    let manifest = rt.block_on(async {
        store
            .get(&ObjectPath::from(manifest_key))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    });
    let m = backup::Manifest::from_json(&manifest).unwrap();
    // The store serve created carries the memory schema: its declarations are
    // WAL frames, so the first restore point has segments.
    assert!(!m.segments.is_empty(), "{m:?}");
}

// ── Graceful shutdown (HO-11) ───────────────────────────────────────────────

use std::io::{Read, Write};
use std::net::TcpStream;

/// The S3-compatible service the tests run against, or `None` (skip).
struct S3Env {
    endpoint: String,
    bucket: String,
    key: String,
    secret: String,
}

impl S3Env {
    fn from_env() -> Option<Self> {
        let Ok(endpoint) = std::env::var("MUSHROOMDB_TEST_S3_ENDPOINT") else {
            eprintln!("MUSHROOMDB_TEST_S3_ENDPOINT is not set: skipping the S3 test");
            return None;
        };
        Some(Self {
            endpoint,
            bucket: std::env::var("MUSHROOMDB_TEST_S3_BUCKET")
                .unwrap_or_else(|_| "mushroomdb-test".into()),
            key: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".into()),
            secret: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into()),
        })
    }

    fn url(&self, prefix: &str) -> String {
        format!("minio://{}/{prefix}", self.bucket)
    }

    fn store(&self, prefix: &str) -> std::sync::Arc<dyn backup::object_store::ObjectStore> {
        let lookup = |k: &str| match k {
            "AWS_ENDPOINT_URL" => Some(self.endpoint.clone()),
            "AWS_ACCESS_KEY_ID" => Some(self.key.clone()),
            "AWS_SECRET_ACCESS_KEY" => Some(self.secret.clone()),
            _ => None,
        };
        backup::store::build_store_with(&backup::BackupConfig::new(self.url(prefix)), lookup)
            .unwrap()
    }

    /// `serve` against `dir`, backing up to `prefix`, stdout piped.
    fn serve(&self, dir: &PathBuf, prefix: &str) -> std::process::Child {
        self.serve_with(dir, prefix, &[], &[])
    }

    /// As [`serve`](Self::serve), with extra arguments and environment.
    fn serve_with(
        &self,
        dir: &PathBuf,
        prefix: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> std::process::Child {
        let url = self.url(prefix);
        let mut all: Vec<(&str, &str)> = vec![
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "100"),
            ("AWS_ENDPOINT_URL", self.endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", self.key.as_str()),
            ("AWS_SECRET_ACCESS_KEY", self.secret.as_str()),
        ];
        all.extend_from_slice(env);
        serve(dir, &all)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mushroomdb serve")
    }
}

/// The address from the `listening on` line.
fn listening_addr(child: &mut std::process::Child) -> String {
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = match lines.next() {
        Some(Ok(l)) if l.starts_with("listening on http://") => {
            l["listening on http://".len()..].trim().to_string()
        }
        other => panic!("serve did not start: {other:?}"),
    };
    // Keep draining so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    addr
}

fn http(addr: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect to serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"));
    let body = raw
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (status, body)
}

fn create_node(addr: &str, key: &str) {
    let (status, body) = http(
        addr,
        "POST",
        "/nodes",
        &format!(r#"{{"label":"Thing","key":"{key}"}}"#),
    );
    assert_eq!(status, 200, "create {key}: {body}");
}

/// SIGTERM, then wait for the process to exit; returns how long that took.
fn terminate(child: &mut std::process::Child) -> Duration {
    let started = std::time::Instant::now();
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success());
    let deadline = started + Duration::from_secs(60);
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success(), "serve exited with {exit}");
            return started.elapsed();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "serve did not exit within 60 s of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn wait_for_object(
    store: &dyn backup::object_store::ObjectStore,
    key: &str,
    within: Duration,
) -> Vec<u8> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Ok(r) = store.get(&ObjectPath::from(key)).await {
            return r.bytes().await.unwrap().to_vec();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no {key} in the bucket"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn shutdown_deletes_lease_and_uploads_final_manifest() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("shutdown");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut child = s3.serve(&dir, &prefix);
    let addr = listening_addr(&mut child);
    for key in ["a", "b", "c"] {
        create_node(&addr, key);
    }
    // The lease exists while the holder runs.
    rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(20),
    ));
    // Write again right before the signal: this commit has not been shipped
    // yet, so only the shutdown flush can carry it.
    create_node(&addr, "d");
    terminate(&mut child);

    let gone = rt.block_on(store.get(&ObjectPath::from(format!("{prefix}/LEASE"))));
    assert!(
        matches!(gone, Err(backup::object_store::Error::NotFound { .. })),
        "the lease must be deleted by a clean shutdown: {gone:?}"
    );
    let latest = rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LATEST"),
        Duration::from_secs(5),
    ));
    let manifest = rt.block_on(wait_for_object(
        store.as_ref(),
        &String::from_utf8(latest).unwrap(),
        Duration::from_secs(5),
    ));
    let m = backup::Manifest::from_json(&manifest).unwrap();
    let reopened = mushroomdb::GraphDb::open(&dir).unwrap();
    for key in ["a", "b", "c", "d"] {
        assert!(reopened.has_node(key), "{key} is in the local store");
    }
    assert_eq!(
        m.commit_seq,
        reopened.wal_total_commits().unwrap(),
        "the final manifest covers the store's last commit"
    );
}

#[test]
fn a_successor_takes_the_lease_at_once_after_a_clean_shutdown() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("successor");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let lease_key = format!("{prefix}/LEASE");
    let instance_of = |bytes: &[u8]| -> String {
        let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        v["instance_id"].as_str().unwrap().to_string()
    };

    let mut first = s3.serve(&dir, &prefix);
    let _ = listening_addr(&mut first);
    let first_id = instance_of(&rt.block_on(wait_for_object(
        store.as_ref(),
        &lease_key,
        Duration::from_secs(20),
    )));
    terminate(&mut first);

    // The default TTL is 30 s and a waiting instance polls every third of it:
    // a successor that finds a fresh lease needs ~10-40 s. One that finds the
    // lease deleted holds it within a few seconds.
    let mut second = s3.serve(&dir, &prefix);
    let _ = listening_addr(&mut second);
    let second_id = instance_of(&rt.block_on(wait_for_object(
        store.as_ref(),
        &lease_key,
        Duration::from_secs(8),
    )));
    assert_ne!(first_id, second_id, "the successor holds its own lease");
    terminate(&mut second);
}

#[test]
fn a_backup_waiting_on_a_foreign_lease_never_gates_ready() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("ready");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Another instance holds a fresh lease: this server's backup can upload
    // nothing and reports itself unhealthy.
    let stamp = String::from_utf8(
        Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let stamp = stamp.trim();
    let record = format!(
        r#"{{"instance_id":"someone-else","acquired_at":"{stamp}","renewed_at":"{stamp}","ttl_s":600}}"#
    );
    rt.block_on(store.put(
        &ObjectPath::from(format!("{prefix}/LEASE")),
        record.into_bytes().into(),
    ))
    .unwrap();

    let mut child = s3.serve(&dir, &prefix);
    let addr = listening_addr(&mut child);
    std::thread::sleep(Duration::from_secs(2));
    let (status, body) = http(&addr, "GET", "/ready", "");
    assert_eq!(status, 200, "/ready must not follow backup health: {body}");
    create_node(&addr, "still-serving");
    // Nothing was uploaded: the lease is still the foreign one.
    let lease = rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(1),
    ));
    assert!(String::from_utf8_lossy(&lease).contains("someone-else"));
    let (status, _) = http(&addr, "GET", "/ready", "");
    assert_eq!(status, 200);
    terminate(&mut child);
    // Shutdown with a lease we never held leaves the foreign lease alone.
    let lease = rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(1),
    ));
    assert!(String::from_utf8_lossy(&lease).contains("someone-else"));
}

/// A bucket that accepts connections and never answers must not hold a
/// shutdown past the backup's bound (flush grace + one request deadline + 2 s).
#[test]
fn a_black_holed_bucket_does_not_hold_up_a_shutdown() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let held = std::thread::spawn(move || {
        // Keep every connection open and silent for the life of the test.
        let mut open = Vec::new();
        for stream in listener.incoming().take(64).flatten() {
            open.push(stream);
        }
    });
    let dir = tmp("blackhole");
    let mut child = serve(
        &dir,
        &[
            ("MUSHROOMDB_BACKUP_S3_URL", "minio://bucket/prefix"),
            ("MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS", "500"),
            ("MUSHROOMDB_BACKUP_OP_TIMEOUT_MS", "500"),
            ("AWS_ENDPOINT_URL", endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", "k"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn mushroomdb serve");
    let addr = listening_addr(&mut child);
    create_node(&addr, "a");
    let took = terminate(&mut child);
    // Budget 3 s, plus the drain and the snapshot of an empty store.
    assert!(took < Duration::from_secs(8), "shutdown took {took:?}");
    drop(held);
}

/// Open a request that never finishes: the headers promise a body, one byte
/// arrives, and the socket stays open. The server cannot drain it.
fn stuck_request(addr: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect to serve");
    stream
        .write_all(
            b"POST /nodes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
              Content-Length: 1000\r\n\r\n{",
        )
        .unwrap();
    stream.flush().unwrap();
    stream
}

fn send_term(child: &std::process::Child) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(status.success());
}

/// Wait for exit and return how long it took; any exit status.
fn wait_exit(child: &mut std::process::Child, started: std::time::Instant, within: Duration) {
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        assert!(
            started.elapsed() < within,
            "serve was still running {:?} after the signal",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

const SMALL_BACKUP_BOUNDS: [(&str, &str); 2] = [
    ("MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS", "500"),
    ("MUSHROOMDB_BACKUP_OP_TIMEOUT_MS", "500"),
];

#[test]
fn a_stuck_request_does_not_stop_shutdown_finishing_within_the_deadline() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("stuck");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Backup bounds 3 s, deadline 10 s: the drain gets 7 s.
    let mut child = s3.serve_with(
        &dir,
        &prefix,
        &["--shutdown-timeout-secs", "10"],
        &SMALL_BACKUP_BOUNDS,
    );
    let addr = listening_addr(&mut child);
    create_node(&addr, "acked");
    rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(20),
    ));
    let _stuck = stuck_request(&addr);
    std::thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    send_term(&child);
    wait_exit(&mut child, started, Duration::from_secs(13));
    let gone = rt.block_on(store.get(&ObjectPath::from(format!("{prefix}/LEASE"))));
    assert!(
        matches!(gone, Err(backup::object_store::Error::NotFound { .. })),
        "the lease is released even though the drain timed out: {gone:?}"
    );
    assert!(mushroomdb::GraphDb::open(&dir).unwrap().has_node("acked"));
}

#[test]
fn a_second_signal_exits_immediately() {
    let dir = tmp("second-signal");
    let mut child = serve(&dir, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    let addr = listening_addr(&mut child);
    let _stuck = stuck_request(&addr);
    std::thread::sleep(Duration::from_millis(300));
    send_term(&child);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the stuck request holds the drain open"
    );
    let started = std::time::Instant::now();
    send_term(&child);
    wait_exit(&mut child, started, Duration::from_secs(3));
}

#[test]
fn a_snapshot_that_overruns_is_skipped_and_the_lease_is_still_released() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("slow-snapshot");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut env = SMALL_BACKUP_BOUNDS.to_vec();
    env.push(("MUSHROOMDB_TEST_SHUTDOWN_SNAPSHOT_DELAY_MS", "60000"));
    // Deadline 10 s, backup reserve 3 s: the snapshot gets 7 s of a 60 s job.
    let mut child = s3.serve_with(&dir, &prefix, &["--shutdown-timeout-secs", "10"], &env);
    let addr = listening_addr(&mut child);
    create_node(&addr, "acked");
    rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(20),
    ));
    let started = std::time::Instant::now();
    send_term(&child);
    wait_exit(&mut child, started, Duration::from_secs(13));
    let gone = rt.block_on(store.get(&ObjectPath::from(format!("{prefix}/LEASE"))));
    assert!(
        matches!(gone, Err(backup::object_store::Error::NotFound { .. })),
        "the lease is released after a skipped snapshot: {gone:?}"
    );
    // No snapshot was written, and the commit is still in the WAL.
    assert!(
        !dir.join("snapshot.bin").exists(),
        "the snapshot was skipped"
    );
    assert!(mushroomdb::GraphDb::open(&dir).unwrap().has_node("acked"));
}

#[test]
fn writes_acknowledged_before_sigterm_are_present_after_restart() {
    let dir = tmp("acked");
    let mut child = serve(&dir, &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb serve");
    let addr = listening_addr(&mut child);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let addr = addr.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut acked = Vec::new();
                for n in 0u32.. {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let key = format!("w{w}-{n}");
                    let Ok(mut stream) = TcpStream::connect(&addr) else {
                        break;
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let body = format!(r#"{{"label":"Thing","key":"{key}"}}"#);
                    let req = format!(
                        "POST /nodes HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(req.as_bytes()).is_err() {
                        break;
                    }
                    let mut raw = String::new();
                    let _ = stream.read_to_string(&mut raw);
                    if raw.starts_with("HTTP/1.1 200") {
                        acked.push(key);
                    }
                }
                acked
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(800));
    let started = std::time::Instant::now();
    send_term(&child);
    wait_exit(&mut child, started, Duration::from_secs(30));
    stop.store(true, std::sync::atomic::Ordering::Release);
    let acked: Vec<String> = writers
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    assert!(!acked.is_empty(), "the writers acknowledged nothing");
    let reopened = mushroomdb::GraphDb::open(&dir).unwrap();
    for key in &acked {
        assert!(reopened.has_node(key), "{key} was acknowledged but is gone");
    }
}
