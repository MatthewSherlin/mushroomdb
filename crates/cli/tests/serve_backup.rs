//! `mushroomdb serve` starts continuous backup when `MUSHROOMDB_BACKUP_S3_URL`
//! is set, and refuses to start when it is set wrong; `mushroomdb restore
//! --from` reads the backup back.
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

    // And `restore --from` reads it back into a store that opens.
    let into = tmp("restored");
    let restore = |into: &PathBuf| {
        Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .args(["restore", "--from", url.as_str(), "--into"])
            .arg(into)
            .env("AWS_ENDPOINT_URL", &endpoint)
            .env("AWS_ACCESS_KEY_ID", &key)
            .env("AWS_SECRET_ACCESS_KEY", &secret)
            .output()
            .expect("run mushroomdb restore")
    };
    let out = restore(&into);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("new prefix"), "{stdout}");
    let restored = mushroomdb::GraphDb::open(&into).unwrap();
    assert!(restored.wal_total_commits().unwrap() >= m.commit_seq);
    drop(restored);
    // A second restore into the same, now full, directory is refused.
    let again = restore(&into);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("not empty"));
}

#[test]
fn restore_refuses_an_unusable_url_and_names_the_flag() {
    let into = tmp("restore-bad");
    for (args, needle) in [
        (vec!["restore"], "--from"),
        (vec!["restore", "--from", "gs://b/p"], "--from"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .args(&args)
            .arg("--into")
            .arg(&into)
            .output()
            .expect("run mushroomdb restore");
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    }
    assert!(!into.exists());
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

/// A TCP endpoint that accepts connections and never answers.
fn black_hole() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut open = Vec::new();
        for stream in listener.incoming().take(256).flatten() {
            open.push(stream);
        }
    });
    endpoint
}

/// Every phase slow at once, under a short `--shutdown-timeout-secs`: a stuck
/// request holds the drain, the snapshot is slow, and the bucket never
/// answers. The default backup bounds (22 s) are larger than the whole
/// window; the sequence must still end inside it.
#[test]
fn a_short_timeout_with_every_phase_slow_still_finishes_inside_it() {
    let endpoint = black_hole();
    let dir = tmp("all-slow");
    let mut child = serve(
        &dir,
        &[
            ("MUSHROOMDB_BACKUP_S3_URL", "minio://bucket/prefix"),
            ("MUSHROOMDB_TEST_SHUTDOWN_SNAPSHOT_DELAY_MS", "60000"),
            ("AWS_ENDPOINT_URL", endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", "k"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ],
    )
    .args(["--shutdown-timeout-secs", "8"])
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn mushroomdb serve");
    let addr = listening_addr(&mut child);
    create_node(&addr, "acked");
    let _stuck = stuck_request(&addr);
    std::thread::sleep(Duration::from_millis(300));
    let started = std::time::Instant::now();
    send_term(&child);
    wait_exit(&mut child, started, Duration::from_secs(11));
    assert!(mushroomdb::GraphDb::open(&dir).unwrap().has_node("acked"));
}

/// A forced exit during a stuck drain and, separately, during a slow
/// snapshot loses nothing that was acknowledged: the WAL is fsynced first.
#[test]
fn an_acknowledged_write_survives_a_forced_second_signal_exit() {
    for (name, stuck, delay) in [
        ("forced-drain", true, None),
        ("forced-snap", false, Some("60000")),
    ] {
        let dir = tmp(name);
        let mut cmd = serve(&dir, &[]);
        if let Some(ms) = delay {
            cmd.env("MUSHROOMDB_TEST_SHUTDOWN_SNAPSHOT_DELAY_MS", ms);
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mushroomdb serve");
        let addr = listening_addr(&mut child);
        for key in ["a", "b", "c"] {
            create_node(&addr, key);
        }
        let _stuck = stuck.then(|| stuck_request(&addr));
        std::thread::sleep(Duration::from_millis(300));
        send_term(&child);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "{name}: still stopping"
        );
        let started = std::time::Instant::now();
        send_term(&child);
        wait_exit(&mut child, started, Duration::from_secs(3));
        let reopened = mushroomdb::GraphDb::open(&dir).unwrap();
        for key in ["a", "b", "c"] {
            assert!(
                reopened.has_node(key),
                "{name}: {key} was acknowledged but is gone"
            );
        }
    }
}

// ── Restore on boot (HO-10) ─────────────────────────────────────────────────

/// A serve, stdout and stderr piped, with `env` added to the S3 credentials.
/// `MUSHROOMDB_RESTORE_FROM_S3` and `MUSHROOMDB_BACKUP_S3_URL` come from
/// `env` only.
fn boot_serve(s3: Option<&S3Env>, dir: &PathBuf, env: &[(&str, &str)]) -> std::process::Child {
    let mut all: Vec<(&str, &str)> = Vec::new();
    if let Some(s3) = s3 {
        all.extend([
            ("AWS_ENDPOINT_URL", s3.endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", s3.key.as_str()),
            ("AWS_SECRET_ACCESS_KEY", s3.secret.as_str()),
            ("MUSHROOMDB_BACKUP_WAL_INTERVAL_MS", "100"),
            ("MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S", "0"),
        ]);
    }
    all.extend_from_slice(env);
    serve(dir, &all)
        .env_remove("MUSHROOMDB_RESTORE_FROM_S3")
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mushroomdb serve")
}

/// Collect a child's stderr on a thread; join for the text.
fn stderr_of(child: &mut std::process::Child) -> std::thread::JoinHandle<String> {
    let mut err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    })
}

/// Wait for a child that must not start serving; returns its stdout.
fn exits_without_serving(child: &mut std::process::Child) -> (std::process::ExitStatus, String) {
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serve kept running");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    (status, out)
}

/// Every object under `prefix` but `LATEST` and `LEASE`, with its bytes.
fn chain_of(
    rt: &tokio::runtime::Runtime,
    store: &dyn backup::object_store::ObjectStore,
    prefix: &str,
) -> std::collections::BTreeMap<String, Vec<u8>> {
    rt.block_on(async {
        let mut out = std::collections::BTreeMap::new();
        let mut dirs = vec![ObjectPath::from(prefix)];
        while let Some(d) = dirs.pop() {
            let r = store.list_with_delimiter(Some(&d)).await.unwrap();
            for o in r.objects {
                let k = o.location.to_string();
                if k.ends_with("/LATEST") || k.ends_with("/LEASE") {
                    continue;
                }
                let b = store.get(&o.location).await.unwrap().bytes().await.unwrap();
                out.insert(k, b.to_vec());
            }
            dirs.extend(r.common_prefixes);
        }
        out
    })
}

fn health_nodes(addr: &str) -> u64 {
    let (status, body) = http(addr, "GET", "/health", "");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["nodes"].as_u64().unwrap_or_else(|| panic!("{body}"))
}

/// An empty data directory restores the newest restore point at boot,
/// before the port is bound: `/ready` is 200 with the restored nodes. The
/// restored store is a new lineage; its backup continues under the same
/// prefix, holding the lease the boot took; and the parent's chain is
/// untouched — its newest restore point still restores the source.
#[test]
fn restore_on_boot_empty_dir() {
    let Some(s3) = S3Env::from_env() else { return };
    let src = tmp("boot-src");
    let prefix = src.file_name().unwrap().to_string_lossy().to_string();
    let url = s3.url(&prefix);
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut first = s3.serve(&src, &prefix);
    let addr = listening_addr(&mut first);
    for key in ["a", "b", "c"] {
        create_node(&addr, key);
    }
    terminate(&mut first);
    let source = mushroomdb::GraphDb::open(&src).unwrap();
    let source_nodes = source.node_count() as u64;
    let source_lineage = std::fs::read_to_string(src.join("backup.lineage")).unwrap();
    drop(source);
    let parent_latest = rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LATEST"),
        Duration::from_secs(5),
    ));
    let parent = chain_of(&rt, store.as_ref(), &prefix);

    let dir = tmp("boot-dir");
    let mut child = boot_serve(
        Some(&s3),
        &dir,
        &[
            ("MUSHROOMDB_RESTORE_FROM_S3", url.as_str()),
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
        ],
    );
    let err = stderr_of(&mut child);
    let addr = listening_addr(&mut child);
    let (status, body) = http(&addr, "GET", "/ready", "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(health_nodes(&addr), source_nodes, "the restored nodes");
    let lineage = std::fs::read_to_string(dir.join("backup.lineage")).unwrap();
    assert_ne!(lineage.trim(), source_lineage.trim(), "a new lineage");
    let lease_of = |bytes: &[u8]| -> (String, String) {
        let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        (
            v["instance_id"].as_str().unwrap().to_string(),
            v["acquired_at"].as_str().unwrap().to_string(),
        )
    };
    let lease = lease_of(&rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(5),
    )));

    // A write ships under the same prefix, as the new lineage.
    create_node(&addr, "after-restore");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let latest = rt.block_on(wait_for_object(
            store.as_ref(),
            &format!("{prefix}/LATEST"),
            Duration::from_secs(5),
        ));
        if latest != parent_latest {
            let m = rt.block_on(wait_for_object(
                store.as_ref(),
                &String::from_utf8(latest).unwrap(),
                Duration::from_secs(5),
            ));
            assert_eq!(
                backup::Manifest::from_json(&m).unwrap().lineage,
                lineage.trim()
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "nothing shipped");
        std::thread::sleep(Duration::from_millis(100));
    }
    let held = lease_of(&rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LEASE"),
        Duration::from_secs(5),
    )));
    assert_eq!(held, lease, "the boot's lease, never taken again");
    terminate(&mut child);
    let stderr = err.join().unwrap();
    assert!(stderr.contains("restore-on-boot: restored"), "{stderr}");
    for secret in [s3.key.as_str(), s3.secret.as_str()] {
        assert!(!stderr.contains(secret), "a credential was logged");
    }

    // The parent's chain, byte for byte, and it still restores the source.
    let now = chain_of(&rt, store.as_ref(), &prefix);
    for (k, bytes) in &parent {
        assert_eq!(now.get(k), Some(bytes), "{k} of the parent changed or went");
    }
    // A copy of the parent's chain alone, LATEST naming its newest manifest.
    let replica: std::sync::Arc<dyn backup::object_store::ObjectStore> =
        std::sync::Arc::new(backup::object_store::memory::InMemory::new());
    let into = tmp("boot-parent");
    let out = rt.block_on(async {
        for (k, bytes) in &parent {
            replica
                .put(&ObjectPath::from(k.as_str()), bytes.clone().into())
                .await
                .unwrap();
        }
        replica
            .put(
                &ObjectPath::from(format!("{prefix}/LATEST")),
                parent_latest.clone().into(),
            )
            .await
            .unwrap();
        backup::restore_from_store(std::sync::Arc::clone(&replica), &prefix, None, &into).await
    });
    assert_eq!(out.unwrap().source_lineage, source_lineage.trim());
    let restored = mushroomdb::GraphDb::open(&into).unwrap();
    let source = mushroomdb::GraphDb::open(&src).unwrap();
    assert_eq!(
        restored.all_nodes_for_export(),
        source.all_nodes_for_export()
    );
    assert_eq!(
        restored.wal_total_commits().unwrap(),
        source.wal_total_commits().unwrap()
    );
}

/// A data directory that holds a store is never restored over, and the
/// bucket is not even contacted: the endpoint here never answers.
#[test]
fn restore_on_boot_nonempty_dir() {
    let dir = tmp("boot-nonempty");
    {
        let mut db = mushroomdb::GraphDb::open(&dir).unwrap();
        db.insert_node("Local", "mine", vec![]).unwrap();
    }
    let endpoint = black_hole();
    let mut child = boot_serve(
        None,
        &dir,
        &[
            ("MUSHROOMDB_RESTORE_FROM_S3", "minio://bucket/prefix"),
            ("AWS_ENDPOINT_URL", endpoint.as_str()),
            ("AWS_ACCESS_KEY_ID", "k"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ],
    );
    let err = stderr_of(&mut child);
    let addr = listening_addr(&mut child);
    assert_eq!(health_nodes(&addr), 1);
    terminate(&mut child);
    let stderr = err.join().unwrap();
    assert!(stderr.contains("already holds a store"), "{stderr}");
    assert!(mushroomdb::GraphDb::open(&dir).unwrap().has_node("mine"));
}

/// A backup that cannot be restored (LATEST names a manifest that is not
/// there) exits 1 without serving, and leaves the data directory without a
/// store.
#[test]
fn restore_on_boot_failure_exits_nonzero() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("boot-broken");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(
        store.put(
            &ObjectPath::from(format!("{prefix}/LATEST")),
            format!("{prefix}/manifest/00000000000000000009.json")
                .into_bytes()
                .into(),
        ),
    )
    .unwrap();
    let url = s3.url(&prefix);
    let mut child = boot_serve(
        Some(&s3),
        &dir,
        &[
            ("MUSHROOMDB_RESTORE_FROM_S3", url.as_str()),
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
        ],
    );
    let err = stderr_of(&mut child);
    let (status, stdout) = exits_without_serving(&mut child);
    assert_eq!(status.code(), Some(1), "{stdout}");
    assert!(!stdout.contains("listening on"), "{stdout}");
    let stderr = err.join().unwrap();
    assert!(stderr.contains("restore-on-boot"), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!mushroomdb::restore::holds_a_store(&dir));
    // The lease is handed back for a successor.
    let gone = rt.block_on(store.get(&ObjectPath::from(format!("{prefix}/LEASE"))));
    assert!(matches!(
        gone,
        Err(backup::object_store::Error::NotFound { .. })
    ));
}

/// A prefix with no backup at all: a warning, and an empty store serves —
/// the first deploy of a template.
#[test]
fn restore_on_boot_empty_bucket_starts_an_empty_store() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("boot-first");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let url = s3.url(&prefix);
    let mut child = boot_serve(
        Some(&s3),
        &dir,
        &[
            ("MUSHROOMDB_RESTORE_FROM_S3", url.as_str()),
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
        ],
    );
    let err = stderr_of(&mut child);
    let addr = listening_addr(&mut child);
    assert_eq!(http(&addr, "GET", "/ready", "").0, 200);
    assert_eq!(health_nodes(&addr), 0);
    create_node(&addr, "first");
    terminate(&mut child);
    let stderr = err.join().unwrap();
    assert!(stderr.contains("holds no backup"), "{stderr}");
    assert!(stderr.contains("empty store"), "{stderr}");
}

/// Another instance's fresh lease: exit 1 naming the holder, nothing
/// written to the data directory.
#[test]
fn restore_on_boot_foreign_lease_exits_nonzero() {
    let Some(s3) = S3Env::from_env() else { return };
    let dir = tmp("boot-leased");
    let prefix = dir.file_name().unwrap().to_string_lossy().to_string();
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
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
        r#"{{"instance_id":"the-live-one","acquired_at":"{stamp}","renewed_at":"{stamp}","ttl_s":600}}"#
    );
    rt.block_on(store.put(
        &ObjectPath::from(format!("{prefix}/LEASE")),
        record.into_bytes().into(),
    ))
    .unwrap();
    let url = s3.url(&prefix);
    let mut child = boot_serve(
        Some(&s3),
        &dir,
        &[("MUSHROOMDB_RESTORE_FROM_S3", url.as_str())],
    );
    let err = stderr_of(&mut child);
    let (status, stdout) = exits_without_serving(&mut child);
    assert_eq!(status.code(), Some(1), "{stdout}");
    let stderr = err.join().unwrap();
    assert!(stderr.contains("the-live-one"), "{stderr}");
    assert!(!dir.exists(), "nothing was written to the data directory");
}

/// Restore on boot does not combine with the other ways of filling an empty
/// directory, and a malformed URL names the variable: both before anything
/// is touched.
#[test]
fn restore_on_boot_refuses_a_bad_setting_before_anything() {
    let dir = tmp("boot-bad");
    for (args, env, needle) in [
        (vec!["--demo-if-empty"], "minio://b/p", "--demo-if-empty"),
        (
            vec!["--restore-from", "/nonexistent"],
            "minio://b/p",
            "--restore-from",
        ),
        (vec![], "gs://b/p", "MUSHROOMDB_RESTORE_FROM_S3"),
    ] {
        let mut child = serve(&dir, &[("MUSHROOMDB_RESTORE_FROM_S3", env)])
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mushroomdb serve");
        let err = stderr_of(&mut child);
        let (status, _) = exits_without_serving(&mut child);
        assert_eq!(status.code(), Some(1), "{args:?}");
        let stderr = err.join().unwrap();
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
        assert!(!dir.exists(), "{args:?}");
    }
}

/// A data directory a restore on boot stopped in the middle of (its install
/// marker is there) holds a part of a store. Without restore on boot to
/// finish it, serve refuses it rather than open it.
#[test]
fn a_half_installed_restore_is_not_served() {
    let dir = tmp("boot-half");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("snapshot.bin"), b"part of a store").unwrap();
    std::fs::write(dir.join(backup::restore::INSTALL_MARKER), "snapshot.bin").unwrap();
    let mut child = boot_serve(None, &dir, &[]);
    let err = stderr_of(&mut child);
    let (status, stdout) = exits_without_serving(&mut child);
    assert_eq!(status.code(), Some(1));
    let stderr = err.join().unwrap();
    assert!(stderr.contains("MUSHROOMDB_RESTORE_FROM_S3"), "{stderr}");
    assert!(!stdout.contains("listening on"));
}

/// M2: not only serve: the query command (and so every command over
/// `GraphDb::open`) refuses a half-installed directory, naming the marker.
#[test]
fn a_half_installed_restore_is_refused_by_query_too() {
    let dir = tmp("boot-half-query");
    {
        let mut db = mushroomdb::GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![]).unwrap();
    }
    std::fs::write(dir.join(backup::restore::INSTALL_MARKER), "snapshot.bin").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("query")
        .arg(&dir)
        .arg("MATCH (n) RETURN count(n)")
        .output()
        .expect("run mushroomdb query");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(backup::restore::INSTALL_MARKER), "{stderr}");
}

/// I1: the lease a restore on boot takes is renewed without a gap through a
/// slow open (one that takes longer than a whole TTL here) into the backup
/// worker, through the real serve path: it is never taken again, no renewal
/// fails or lapses, and the first write ships as soon as the server is up.
#[test]
fn restore_on_boot_holds_the_lease_through_a_slow_open() {
    let Some(s3) = S3Env::from_env() else { return };
    let src = tmp("boot-slow-src");
    let prefix = src.file_name().unwrap().to_string_lossy().to_string();
    let url = s3.url(&prefix);
    let store = s3.store(&prefix);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut first = s3.serve(&src, &prefix);
    let addr = listening_addr(&mut first);
    create_node(&addr, "a");
    terminate(&mut first);
    let parent_latest = rt.block_on(wait_for_object(
        store.as_ref(),
        &format!("{prefix}/LATEST"),
        Duration::from_secs(5),
    ));

    let lease_key = format!("{prefix}/LEASE");
    let read_lease = || -> Option<serde_json::Value> {
        rt.block_on(async {
            match store.get(&ObjectPath::from(lease_key.as_str())).await {
                Ok(r) => Some(serde_json::from_slice(&r.bytes().await.unwrap()).unwrap()),
                Err(_) => None,
            }
        })
    };
    let dir = tmp("boot-slow-dir");
    // TTL 3 s: renewals every 1 s, the write fence at 2 s. The open takes 5 s.
    let mut child = boot_serve(
        Some(&s3),
        &dir,
        &[
            ("MUSHROOMDB_RESTORE_FROM_S3", url.as_str()),
            ("MUSHROOMDB_BACKUP_S3_URL", url.as_str()),
            ("MUSHROOMDB_BACKUP_LEASE_TTL", "3"),
            ("MUSHROOMDB_TEST_OPEN_DELAY_MS", "5000"),
        ],
    );
    let err = stderr_of(&mut child);
    // Watch the lease while the open is held up: it never goes stale.
    let started = std::time::Instant::now();
    let mut seen: Vec<(String, String, String)> = Vec::new();
    while started.elapsed() < Duration::from_secs(6) {
        if let Some(v) = read_lease() {
            let rec = (
                v["instance_id"].as_str().unwrap().to_string(),
                v["acquired_at"].as_str().unwrap().to_string(),
                v["renewed_at"].as_str().unwrap().to_string(),
            );
            if seen.last() != Some(&rec) {
                seen.push(rec);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!seen.is_empty(), "the boot never took the lease");
    let addr = listening_addr(&mut child);
    create_node(&addr, "after");
    let shipped = std::time::Instant::now();
    loop {
        let latest = rt.block_on(wait_for_object(
            store.as_ref(),
            &format!("{prefix}/LATEST"),
            Duration::from_secs(5),
        ));
        if latest != parent_latest {
            break;
        }
        assert!(
            shipped.elapsed() < Duration::from_secs(10),
            "the first write did not ship: the lease's write fence was hit"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let after = read_lease().expect("still held");
    terminate(&mut child);
    let stderr = err.join().unwrap();

    let (id, acquired) = (&seen[0].0, &seen[0].1);
    for (i, a, _) in &seen {
        assert_eq!((i, a), (id, acquired), "taken again: {seen:?}");
    }
    assert_eq!(after["instance_id"].as_str().unwrap(), id);
    assert_eq!(after["acquired_at"].as_str().unwrap(), acquired);
    // Renewed at least every second or so across the 6 s window.
    assert!(seen.len() >= 4, "not renewed through the open: {seen:?}");
    for bad in [
        "renewal failed",
        "lost the backup bucket lease",
        "uploads are stopped",
    ] {
        assert!(!stderr.contains(bad), "{bad}: {stderr}");
    }
}
