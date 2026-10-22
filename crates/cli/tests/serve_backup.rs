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
