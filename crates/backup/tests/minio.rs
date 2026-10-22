//! The worker against a real S3-compatible service.
//!
//! Runs only when `MUSHROOMDB_TEST_S3_ENDPOINT` is set (for example
//! `http://127.0.0.1:9000` for a local MinIO); otherwise every test returns at
//! once. The bucket (`MUSHROOMDB_TEST_S3_BUCKET`, default `mushroomdb-test`)
//! must exist. Credentials come from `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY`, defaulting to MinIO's `minioadmin`. Each run writes
//! under a fresh prefix.

use backup::lease::{lease_key, LeaseRecord};
use backup::manifest::{crc32_hex, latest_key, parse_segment_key};
use backup::store::build_store_with;
use backup::{BackupConfig, BackupWorker, BucketLease, Exclusion, LeaseError, Manifest};
use mushroomdb::{BatchOp, GraphDb, SharedDb, SnapshotOptions, Value};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::path::Path;
use std::sync::Arc;

struct Bucket {
    store: Arc<dyn ObjectStore>,
    config: BackupConfig,
    prefix: String,
}

fn bucket() -> Option<Bucket> {
    bucket_with(true)
}

/// A bucket handle; `lease_conditional` as `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL`.
fn bucket_with(lease_conditional: bool) -> Option<Bucket> {
    let Ok(endpoint) = std::env::var("MUSHROOMDB_TEST_S3_ENDPOINT") else {
        eprintln!("MUSHROOMDB_TEST_S3_ENDPOINT is not set: skipping the S3 test");
        return None;
    };
    let name =
        std::env::var("MUSHROOMDB_TEST_S3_BUCKET").unwrap_or_else(|_| "mushroomdb-test".into());
    let prefix = format!(
        "ho07-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let config = BackupConfig {
        lease_conditional,
        ..BackupConfig::new(format!("minio://{name}/{prefix}"))
    };
    let env = |k: &str| -> Option<String> {
        match k {
            "AWS_ENDPOINT_URL" => Some(endpoint.clone()),
            "AWS_ACCESS_KEY_ID" => Some(std::env::var(k).unwrap_or_else(|_| "minioadmin".into())),
            "AWS_SECRET_ACCESS_KEY" => {
                Some(std::env::var(k).unwrap_or_else(|_| "minioadmin".into()))
            }
            _ => std::env::var(k).ok(),
        }
    };
    let store = build_store_with(&config, env).expect("the S3 client builds");
    Some(Bucket {
        store,
        config,
        prefix,
    })
}

async fn get(store: &Arc<dyn ObjectStore>, key: &str) -> Vec<u8> {
    store
        .get(&ObjectPath::from(key))
        .await
        .unwrap_or_else(|e| panic!("get {key}: {e}"))
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

async fn restore_latest(b: &Bucket, into: &Path) -> Manifest {
    let key = String::from_utf8(get(&b.store, &latest_key(&b.prefix)).await).unwrap();
    let m = Manifest::from_json(&get(&b.store, &key).await).unwrap();
    for f in &m.files {
        let bytes = get(&b.store, &f.key).await;
        assert_eq!(
            (bytes.len() as u64, crc32_hex(&bytes)),
            (f.size, f.crc32.clone())
        );
        std::fs::write(into.join(&f.name), bytes).unwrap();
    }
    let mut wal = Vec::new();
    for s in &m.segments {
        let (_, start, _) = parse_segment_key(&b.prefix, &s.key).unwrap();
        assert_eq!(start, wal.len() as u64, "contiguous segments");
        let bytes = get(&b.store, &s.key).await;
        assert_eq!(crc32_hex(&bytes), s.crc32);
        wal.extend(bytes);
    }
    std::fs::write(into.join("wal.bin"), wal).unwrap();
    m
}

#[tokio::test]
async fn s3_round_trip_with_a_multipart_snapshot() {
    let Some(b) = bucket() else { return };
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    // ~13 MiB of incompressible property data: the snapshot crosses the
    // 10 MiB single-PUT threshold and goes up as a multipart upload.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut noise = |n: usize| -> String {
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                char::from(b'a' + (state % 26) as u8)
            })
            .collect()
    };
    let ops: Vec<BatchOp> = (0..200)
        .map(|i| BatchOp::InsertNode {
            label: "A".into(),
            key: format!("n{i}"),
            props: vec![("blob".into(), Value::Str(noise(64 * 1024)))],
        })
        .collect();
    db.submit_batch(ops).unwrap();
    let mut w = BackupWorker::new(
        b.config.clone(),
        Arc::clone(&b.store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    w.run_once().await.unwrap();
    db.write()
        .snapshot_with(SnapshotOptions {
            keep_wal: false,
            archive_wal: true,
        })
        .unwrap();
    for i in 0..10 {
        db.write()
            .insert_node("B", &format!("m{i}"), vec![])
            .unwrap();
    }
    w.run_once().await.unwrap();
    w.flush().await;

    let restored = tempfile::tempdir().unwrap();
    let m = restore_latest(&b, restored.path()).await;
    assert!(m
        .files
        .iter()
        .any(|f| f.name == "snapshot.bin" && f.size > 10 << 20));
    assert!(!m.segments.is_empty());
    let r = GraphDb::open(restored.path()).unwrap();
    assert_eq!(r.node_count(), db.read().node_count());
    assert_eq!(r.node_count(), 210);
}

/// A refused request's body can carry the access key id; the error the worker
/// logs must not.
#[tokio::test]
async fn s3_refusals_are_logged_without_the_access_key_id() {
    let Ok(endpoint) = std::env::var("MUSHROOMDB_TEST_S3_ENDPOINT") else {
        eprintln!("MUSHROOMDB_TEST_S3_ENDPOINT is not set: skipping the S3 test");
        return;
    };
    let name =
        std::env::var("MUSHROOMDB_TEST_S3_BUCKET").unwrap_or_else(|_| "mushroomdb-test".into());
    let config = BackupConfig::new(format!("minio://{name}/ho07-refused"));
    let key_id = "AKIAIOSFODNN7EXAMPLE";
    let env = |k: &str| match k {
        "AWS_ENDPOINT_URL" => Some(endpoint.clone()),
        "AWS_ACCESS_KEY_ID" => Some(key_id.to_string()),
        "AWS_SECRET_ACCESS_KEY" => Some("not-the-secret".to_string()),
        _ => None,
    };
    let store = build_store_with(&config, env).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let mut w = BackupWorker::new(
        BackupConfig {
            op_timeout_ms: 5_000,
            ..config
        },
        store,
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let err = w
        .run_once()
        .await
        .expect_err("the bucket refuses unknown keys");
    let shown = err.to_string();
    assert!(!shown.contains(key_id), "{shown}");
}

/// On a real S3 service the lease is a compare-and-swap: contenders that
/// start together get exactly one lease, the probe confirms the service
/// honors preconditions, and a holder whose lease was replaced learns so at
/// its next renewal and leaves the new lease alone on release.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_lease_gives_real_exclusion() {
    let Some(b) = bucket() else { return };
    let tries: Vec<_> = (0..8)
        .map(|_| {
            let (s, p) = (Arc::clone(&b.store), b.prefix.clone());
            tokio::spawn(async move { BucketLease::acquire(s, &p, 30).await })
        })
        .collect();
    let mut winners = Vec::new();
    for t in tries {
        match t.await.unwrap() {
            Ok(l) => winners.push(l),
            Err(LeaseError::ForeignActive { .. }) => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one lease");
    let lease = winners.pop().unwrap();
    assert_eq!(lease.exclusion(), Exclusion::Conditional);
    lease.renew().await.unwrap();
    assert_eq!(lease.exclusion(), Exclusion::Conditional, "If-Match works");

    let key = ObjectPath::from(lease_key(&b.prefix));
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let foreign = LeaseRecord {
        instance_id: "successor".into(),
        acquired_at: now.clone(),
        renewed_at: now,
        ttl_s: 30,
    };
    b.store
        .put(
            &key,
            PutPayload::from(serde_json::to_vec(&foreign).unwrap()),
        )
        .await
        .unwrap();
    assert!(matches!(
        lease.renew().await,
        Err(LeaseError::Lost { instance_id: Some(ref id) }) if id == "successor"
    ));
    lease.release().await.unwrap();
    let left: LeaseRecord = serde_json::from_slice(&get(&b.store, key.as_ref()).await).unwrap();
    assert_eq!(left.instance_id, "successor");
    b.store.delete(&key).await.unwrap();
}

/// With `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off` the client sends no
/// preconditions, and the lease says it is best-effort.
#[tokio::test]
async fn s3_lease_without_conditional_put_is_best_effort() {
    let Some(b) = bucket_with(false) else { return };
    let lease = BucketLease::acquire(Arc::clone(&b.store), &b.prefix, 30)
        .await
        .unwrap();
    assert_eq!(lease.exclusion(), Exclusion::BestEffort);
    lease.renew().await.unwrap();
    lease.release().await.unwrap();
}

/// A spawned worker on a real S3 service takes the lease, keeps it across
/// renewals, uploads under it, and deletes it on shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_worker_holds_renews_and_releases_the_lease() {
    let Some(b) = bucket() else { return };
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let (tx, rx) = tokio::sync::watch::channel(backup::BackupTrigger::Idle);
    let h = BackupWorker::spawn(
        BackupConfig {
            wal_interval_ms: 100,
            manifest_interval_s: 0,
            // Generous: a loaded CI runner must not time a request out.
            lease_ttl_s: 6,
            op_timeout_ms: 5_000,
            ..b.config.clone()
        },
        Arc::clone(&b.store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    let key = ObjectPath::from(lease_key(&b.prefix));
    // Across renewals (every two seconds), writes keep shipping.
    for i in 0..4 {
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        db.write()
            .insert_node("A", &format!("n{i}"), vec![])
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let held: LeaseRecord = serde_json::from_slice(&get(&b.store, key.as_ref()).await).unwrap();
    let at = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
    };
    assert!(
        at(&held.renewed_at) > at(&held.acquired_at),
        "renewed: {held:?}"
    );
    assert!(
        h.healthy.load(std::sync::atomic::Ordering::Acquire),
        "{:?}",
        h.reason.lock().unwrap()
    );
    assert_eq!(h.error_count.load(std::sync::atomic::Ordering::Acquire), 0);
    tx.send(backup::BackupTrigger::Shutdown).unwrap();
    h.join.await.unwrap();
    assert!(matches!(
        b.store.head(&key).await,
        Err(object_store::Error::NotFound { .. })
    ));
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&b, restored.path()).await;
    assert_eq!(GraphDb::open(restored.path()).unwrap().node_count(), 5);
}
