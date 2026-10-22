//! A prefix belongs to one store lineage (defect-ledger row 138).
//!
//! A different store, or a store restored from an older backup, pointed at a
//! prefix must not resume there: it would overwrite manifests, have its own
//! backup deleted by retention, or assemble a restore point from another
//! store's archives. And the bucket itself refuses a stale writer's
//! conditional writes.

mod support;

use backup::lease::lease_key;
use backup::manifest::{crc32_hex, latest_key, parse_segment_key};
use backup::{BackupConfig, BackupError, BackupWorker, Manifest};
use mushroomdb::{GraphDb, RealFs, SharedDb, SnapshotOptions};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const PREFIX: &str = "pfx";

const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};

fn mem() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

fn config() -> BackupConfig {
    BackupConfig {
        manifest_interval_s: 0,
        ..BackupConfig::new(format!("s3://test-bucket/{PREFIX}"))
    }
}

fn worker_with(
    store: &Arc<dyn ObjectStore>,
    db: &SharedDb,
    dir: &Path,
    c: BackupConfig,
) -> BackupWorker {
    BackupWorker::new(
        c,
        Arc::clone(store),
        Arc::new(db.clone()),
        dir.to_path_buf(),
    )
    .unwrap()
}

fn worker(store: &Arc<dyn ObjectStore>, db: &SharedDb, dir: &Path) -> BackupWorker {
    worker_with(store, db, dir, config())
}

async fn get(store: &Arc<dyn ObjectStore>, key: &str) -> Option<Vec<u8>> {
    match store.get(&ObjectPath::from(key)).await {
        Ok(r) => Some(r.bytes().await.unwrap().to_vec()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(e) => panic!("get {key}: {e}"),
    }
}

/// Every key under the prefix with its bytes, sorted.
async fn snapshot_of_bucket(store: &Arc<dyn ObjectStore>) -> Vec<(String, Vec<u8>)> {
    let mut keys = Vec::new();
    let mut dirs = vec![ObjectPath::from(PREFIX)];
    while let Some(d) = dirs.pop() {
        let r = store.list_with_delimiter(Some(&d)).await.unwrap();
        keys.extend(r.objects.into_iter().map(|o| o.location.to_string()));
        dirs.extend(r.common_prefixes);
    }
    keys.sort();
    let mut out = Vec::new();
    for k in keys {
        let b = get(store, &k).await.unwrap();
        out.push((k, b));
    }
    out
}

async fn latest(store: &Arc<dyn ObjectStore>) -> Manifest {
    let key = String::from_utf8(get(store, &latest_key(PREFIX)).await.unwrap()).unwrap();
    Manifest::from_json(&get(store, &key).await.expect("LATEST's target exists")).unwrap()
}

/// Rebuild a store from `LATEST` alone, verifying every checksum, and open it.
async fn restore_latest(store: &Arc<dyn ObjectStore>, into: &Path) -> GraphDb<RealFs> {
    let m = latest(store).await;
    for f in &m.files {
        let bytes = get(store, &f.key).await.expect("a named file exists");
        assert_eq!(crc32_hex(&bytes), f.crc32, "{}", f.key);
        std::fs::write(into.join(&f.name), bytes).unwrap();
    }
    let mut wal = Vec::new();
    for s in &m.segments {
        let (_, start, _) = parse_segment_key(PREFIX, &s.key).unwrap();
        assert_eq!(start, wal.len() as u64);
        let bytes = get(store, &s.key).await.expect("a named segment exists");
        assert_eq!(crc32_hex(&bytes), s.crc32);
        wal.extend(bytes);
    }
    std::fs::write(into.join("wal.bin"), wal).unwrap();
    GraphDb::open(into).expect("a restored store opens")
}

/// Copy a closed store's files.
fn copy_store(from: &Path, to: &Path) {
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() && e.file_name() != "LOCK" {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

fn insert(db: &SharedDb, prefix: &str, n: usize) {
    for i in 0..n {
        db.write()
            .insert_node("A", &format!("{prefix}{i}"), vec![])
            .unwrap();
    }
}

fn assert_refused(e: BackupError, needle: &str) {
    match e {
        BackupError::Lineage(m) => assert!(m.contains(needle), "{m}"),
        other => panic!("expected a lineage refusal, got {other}"),
    }
}

// ── Row 138 ──────────────────────────────────────────────────────────────────

/// A different store pointed at a prefix holding another store's backups
/// uploads nothing, deletes nothing, and says why — even with retention due.
#[tokio::test]
async fn a_different_store_taking_over_a_prefix_is_refused() {
    let store = mem();
    let (da, db_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = SharedDb::open(da.path()).unwrap();
    let mut wa = worker_with(
        &store,
        &a,
        da.path(),
        BackupConfig {
            retain_n: 2,
            ..config()
        },
    );
    for round in 0..4 {
        insert(&a, &format!("a{round}-"), 3);
        a.write().snapshot_with(ARCHIVING).unwrap();
        wa.run_once().await.unwrap();
    }
    let before = snapshot_of_bucket(&store).await;

    // Store B: a fresh volume, fewer commits than A, retention due at once.
    let b = SharedDb::open(db_dir.path()).unwrap();
    insert(&b, "b", 2);
    let mut wb = worker_with(
        &store,
        &b,
        db_dir.path(),
        BackupConfig {
            retain_n: 1,
            ..config()
        },
    );
    let c = wb.counters();
    assert_refused(wb.run_once().await.unwrap_err(), "another store");
    wb.step().await;
    assert!(!c.healthy.load(Ordering::Acquire));
    let why = c.reason.lock().unwrap().clone().unwrap();
    assert!(why.contains("new prefix"), "{why}");
    assert_eq!(
        snapshot_of_bucket(&store).await,
        before,
        "the bucket is untouched"
    );
    assert!(
        !db_dir.path().join("backup.lineage").exists(),
        "B is not given a lineage it would then share with nothing"
    );

    // A carries on unaffected, and its latest restores to A.
    insert(&a, "a-late", 1);
    wa.run_once().await.unwrap();
    let r = tempfile::tempdir().unwrap();
    assert_eq!(
        restore_latest(&store, r.path()).await.node_count(),
        a.read().node_count()
    );
}

/// A store restored from an older backup of the same lineage, then pointed
/// back at the prefix, is refused: its restore points would sort below the
/// newer history and be retired, and its manifests would overwrite it.
#[tokio::test]
async fn a_rewound_store_taking_over_is_refused() {
    let store = mem();
    let (da, old) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = SharedDb::open(da.path()).unwrap();
    insert(&a, "a", 5);
    let mut wa = worker(&store, &a, da.path());
    wa.run_once().await.unwrap();
    // Keep a copy of the store as it was at commit 5: an older restore.
    drop(wa);
    drop(a);
    copy_store(da.path(), old.path());
    let a = SharedDb::open(da.path()).unwrap();
    let mut wa = worker(&store, &a, da.path());
    insert(&a, "x", 5);
    wa.run_once().await.unwrap();
    assert_eq!(latest(&store).await.commit_seq, 10);
    let before = snapshot_of_bucket(&store).await;

    let rewound = SharedDb::open(old.path()).unwrap();
    assert!(old.path().join("backup.lineage").exists(), "same lineage");
    let mut w = worker(&store, &rewound, old.path());
    assert_refused(w.run_once().await.unwrap_err(), "fewer than the 10");
    assert_eq!(snapshot_of_bucket(&store).await, before);
}

/// The mixed restore point of row 138: a store of the same lineage whose
/// history diverged has an archive with the same number and length as one
/// in the prefix but different bytes. Number and length used to be enough
/// to reuse the prefix's copy, giving a restore point that verified and
/// restored the other history. The CRC check uploads its own instead.
#[tokio::test]
async fn a_same_size_archive_with_other_bytes_is_uploaded_not_reused() {
    let store = mem();
    let (da, db_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = SharedDb::open(da.path()).unwrap();
    insert(&a, "a", 3);
    worker(&store, &a, da.path()).run_once().await.unwrap();
    drop(a);
    copy_store(da.path(), db_dir.path());

    // A and B diverge by the same number of same-sized commits, then each
    // archives its WAL.
    let a = SharedDb::open(da.path()).unwrap();
    insert(&a, "x", 3);
    a.write().snapshot_with(ARCHIVING).unwrap();
    let b = SharedDb::open(db_dir.path()).unwrap();
    insert(&b, "y", 3);
    b.write().snapshot_with(ARCHIVING).unwrap();
    let archive = |d: &Path| {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".archive"))
            .expect("an archive")
    };
    let (na, nb) = (archive(da.path()), archive(db_dir.path()));
    let (ba, bb) = (
        std::fs::read(da.path().join(&na)).unwrap(),
        std::fs::read(db_dir.path().join(&nb)).unwrap(),
    );
    assert_eq!(na, nb, "setup: the same archive number");
    assert_eq!(ba.len(), bb.len(), "setup: the same length");
    assert_ne!(ba, bb, "setup: different bytes");

    worker(&store, &a, da.path()).run_once().await.unwrap();
    // A was the last writer; B now holds as many commits, so it may resume.
    let mut wb = worker(&store, &b, db_dir.path());
    wb.run_once().await.unwrap();
    let m = latest(&store).await;
    let entry = m.files.iter().find(|f| f.name == nb).unwrap();
    assert_eq!(
        get(&store, &entry.key).await.unwrap(),
        bb,
        "B's own archive"
    );
    let r = tempfile::tempdir().unwrap();
    let restored = restore_latest(&store, r.path()).await;
    assert_eq!(restored.node_count(), b.read().node_count());
    for i in 0..3 {
        assert!(restored.has_node(&format!("y{i}")), "B's history");
        assert!(
            !restored.has_node(&format!("x{i}")),
            "no frame of A's history"
        );
    }
}

/// A manifest in the prefix that mixes two lineages does not parse, so
/// resume refuses to build on it instead of copying its entries.
#[tokio::test]
async fn a_mixed_restore_point_in_the_prefix_fails_to_verify() {
    let store = mem();
    let d = tempfile::tempdir().unwrap();
    let db = SharedDb::open(d.path()).unwrap();
    insert(&db, "a", 2);
    db.write().snapshot_with(ARCHIVING).unwrap();
    worker(&store, &db, d.path()).run_once().await.unwrap();
    let key = String::from_utf8(get(&store, &latest_key(PREFIX)).await.unwrap()).unwrap();
    let mut v: serde_json::Value =
        serde_json::from_slice(&get(&store, &key).await.unwrap()).unwrap();
    v["files"][0]["lineage"] = serde_json::json!("someone-else");
    let bytes = serde_json::to_vec(&v).unwrap();
    let e = Manifest::from_json(&bytes).unwrap_err();
    assert!(e.contains("mixing two stores"), "{e}");
    store
        .put(&ObjectPath::from(key.as_str()), PutPayload::from(bytes))
        .await
        .unwrap();
    insert(&db, "b", 1);
    let mut w = worker(&store, &db, d.path());
    match w.run_once().await {
        Err(BackupError::Manifest(m)) => assert!(m.contains("mixing two stores"), "{m}"),
        other => panic!("expected a manifest error, got {other:?}"),
    }
}

/// Retention never deletes the manifest it just wrote nor `LATEST`'s target,
/// even when a manifest of this lineage with a higher commit (a stale
/// writer's) sorts above it.
#[tokio::test]
async fn retention_keeps_the_manifest_it_just_wrote() {
    let store = mem();
    let d = tempfile::tempdir().unwrap();
    let db = SharedDb::open(d.path()).unwrap();
    let c = BackupConfig {
        retain_n: 1,
        ..config()
    };
    let mut w = worker_with(&store, &db, d.path(), c.clone());
    insert(&db, "a", 2);
    w.run_once().await.unwrap();
    // A stale writer's manifest of this lineage, far ahead.
    let mut ghost = latest(&store).await;
    ghost.commit_seq = 999_999;
    store
        .put(
            &ObjectPath::from(format!("{PREFIX}/manifest/{:020}.json", ghost.commit_seq)),
            PutPayload::from(ghost.to_json()),
        )
        .await
        .unwrap();
    insert(&db, "b", 2);
    w.run_once().await.unwrap();
    let m = latest(&store).await;
    assert_eq!(m.commit_seq, db.read().wal_total_commits().unwrap());
    let r = tempfile::tempdir().unwrap();
    assert_eq!(restore_latest(&store, r.path()).await.node_count(), 4);
}

// ── The bucket refuses a stale writer ───────────────────────────────────────

/// Once another writer has moved `LATEST`, a stale writer's `LATEST` is
/// refused by the bucket (If-Match), and so is an immutable object it would
/// overwrite with other bytes (If-None-Match).
pub async fn stale_writer_is_refused_by_the_bucket(store: Arc<dyn ObjectStore>, prefix: &str) {
    let d = tempfile::tempdir().unwrap();
    let db = SharedDb::open(d.path()).unwrap();
    insert(&db, "a", 2);
    let c = BackupConfig {
        manifest_interval_s: 0,
        ..BackupConfig::new(format!("s3://test-bucket/{prefix}"))
    };
    let mut stale = BackupWorker::new(
        c,
        Arc::clone(&store),
        Arc::new(db.clone()),
        d.path().to_path_buf(),
    )
    .unwrap();
    stale.run_once().await.unwrap();
    // Another writer moves LATEST (a successor's restore point).
    let latest = ObjectPath::from(latest_key(prefix));
    let successor = format!("{prefix}/manifest/successor.json");
    store
        .put(&latest, PutPayload::from(successor.clone().into_bytes()))
        .await
        .unwrap();
    insert(&db, "b", 2);
    match stale.run_once().await {
        Err(BackupError::Conflict(m)) => assert!(m.contains("LATEST"), "{m}"),
        other => panic!("expected the bucket to refuse LATEST, got {other:?}"),
    }
    let now = store.get(&latest).await.unwrap().bytes().await.unwrap();
    assert_eq!(
        now.as_ref(),
        successor.as_bytes(),
        "the successor's LATEST stands"
    );
    let _ = lease_key(prefix);
}

#[tokio::test]
async fn a_stale_writer_is_refused_by_the_bucket() {
    stale_writer_is_refused_by_the_bucket(mem(), PREFIX).await;
}

/// An immutable object already in the bucket with other bytes is never
/// overwritten.
#[tokio::test]
async fn an_immutable_object_is_never_overwritten() {
    let store = mem();
    let d = tempfile::tempdir().unwrap();
    let db = SharedDb::open(d.path()).unwrap();
    insert(&db, "a", 2);
    let mut w = worker(&store, &db, d.path());
    w.run_once().await.unwrap();
    let m = latest(&store).await;
    // Plant other bytes where this worker's next segment will go.
    let (g, _, end) = parse_segment_key(PREFIX, &m.segments.last().unwrap().key).unwrap();
    insert(&db, "b", 1);
    let len = std::fs::metadata(d.path().join("wal.bin")).unwrap().len();
    let next = backup::manifest::segment_key(PREFIX, g, end, len);
    store
        .put(
            &ObjectPath::from(next.as_str()),
            PutPayload::from_static(b"not ours"),
        )
        .await
        .unwrap();
    assert!(matches!(w.run_once().await, Err(BackupError::Conflict(_))));
    assert_eq!(get(&store, &next).await.unwrap(), b"not ours");
}
