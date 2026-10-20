//! The worker against a real S3-compatible service.
//!
//! Runs only when `MUSHROOMDB_TEST_S3_ENDPOINT` is set (for example
//! `http://127.0.0.1:9000` for a local MinIO); otherwise every test returns at
//! once. The bucket (`MUSHROOMDB_TEST_S3_BUCKET`, default `mushroomdb-test`)
//! must exist. Credentials come from `AWS_ACCESS_KEY_ID` /
//! `AWS_SECRET_ACCESS_KEY`, defaulting to MinIO's `minioadmin`. Each run writes
//! under a fresh prefix.

use backup::manifest::{crc32_hex, latest_key, parse_segment_key};
use backup::store::build_store_with;
use backup::{BackupConfig, BackupWorker, Manifest};
use mushroomdb::{BatchOp, GraphDb, SharedDb, SnapshotOptions, Value};
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use std::path::Path;
use std::sync::Arc;

struct Bucket {
    store: Arc<dyn ObjectStore>,
    config: BackupConfig,
    prefix: String,
}

fn bucket() -> Option<Bucket> {
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
    let config = BackupConfig::new(format!("minio://{name}/{prefix}"));
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
        let name = f.key.rsplit('/').next().unwrap();
        std::fs::write(into.join(name), bytes).unwrap();
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
        .any(|f| f.key.ends_with("/snapshot.bin") && f.size > 10 << 20));
    assert!(!m.segments.is_empty());
    let r = GraphDb::open(restored.path()).unwrap();
    assert_eq!(r.node_count(), db.read().node_count());
    assert_eq!(r.node_count(), 210);
}
