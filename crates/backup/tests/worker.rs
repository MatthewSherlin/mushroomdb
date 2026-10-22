//! BackupWorker against an in-memory object store and a real store on disk.
//!
//! Every restore check rebuilds a store from the bucket alone — the latest
//! manifest's files plus its segments concatenated into `wal.bin` — opens it,
//! and compares node and edge counts with the source (Review Focus 1).

use backup::manifest::{crc32_hex, parse_segment_key};
use backup::segment::read_new_segment;
use backup::{
    BackupConfig, BackupTrigger, BackupWorker, FileEntry, Manifest, SegmentEntry, MANIFEST_VERSION,
};
use mushroomdb::{FsyncPolicy, GraphDb, SharedDb, SnapshotOptions};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

mod support;

const PREFIX: &str = "pfx";

fn config(wal_interval_ms: u64) -> BackupConfig {
    BackupConfig {
        wal_interval_ms,
        ..BackupConfig::new(format!("s3://test-bucket/{PREFIX}"))
    }
}

fn mem() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

fn worker(store: &Arc<dyn ObjectStore>, db: &SharedDb, dir: &Path) -> BackupWorker {
    BackupWorker::new(
        config(1000),
        Arc::clone(store),
        Arc::new(db.clone()),
        dir.to_path_buf(),
    )
    .unwrap()
}

/// What an automatic snapshot does: archive the WAL, start a new one.
const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};

async fn get(store: &Arc<dyn ObjectStore>, key: &str) -> Option<Vec<u8>> {
    match store.get(&ObjectPath::from(key)).await {
        Ok(r) => Some(r.bytes().await.unwrap().to_vec()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(e) => panic!("get {key}: {e}"),
    }
}

/// Every key under `prefix`, sorted.
async fn keys(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![ObjectPath::from(PREFIX)];
    while let Some(d) = dirs.pop() {
        let r = store.list_with_delimiter(Some(&d)).await.unwrap();
        out.extend(r.objects.into_iter().map(|o| o.location.to_string()));
        dirs.extend(r.common_prefixes);
    }
    out.sort();
    out
}

async fn segment_keys(store: &Arc<dyn ObjectStore>, generation: u64) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = keys(store)
        .await
        .iter()
        .filter_map(|k| parse_segment_key(PREFIX, k))
        .filter(|(g, _, _)| *g == generation)
        .map(|(_, s, e)| (s, e))
        .collect();
    v.sort();
    v
}

async fn latest(store: &Arc<dyn ObjectStore>) -> Option<Manifest> {
    let key = String::from_utf8(get(store, &format!("{PREFIX}/LATEST")).await?).unwrap();
    let bytes = get(store, &key)
        .await
        .expect("LATEST names a manifest that exists");
    Some(Manifest::from_json(&bytes).unwrap())
}

/// Rebuild a store in `into` from the latest manifest alone, verifying every
/// checksum and that the segments are contiguous from offset 0.
async fn restore_latest(store: &Arc<dyn ObjectStore>, into: &Path) -> Manifest {
    let m = latest(store).await.expect("a manifest was written");
    restore(store, &m, into).await;
    m
}

/// Rebuild a store in `into` from `m`, verifying every checksum, that the
/// segments are contiguous from offset 0, and that the store opened from it
/// holds exactly `m.commit_seq` frames.
async fn restore(store: &Arc<dyn ObjectStore>, m: &Manifest, into: &Path) {
    std::fs::create_dir_all(into).unwrap();
    for f in &m.files {
        let bytes = get(store, &f.key).await.expect("a named file exists");
        assert_eq!(bytes.len() as u64, f.size, "{}", f.key);
        assert_eq!(crc32_hex(&bytes), f.crc32, "{}", f.key);
        assert!(!f.name.contains('/') && !f.name.is_empty(), "{}", f.name);
        std::fs::write(into.join(&f.name), bytes).unwrap();
    }
    let mut wal = Vec::new();
    for s in &m.segments {
        let (g, start, end) = parse_segment_key(PREFIX, &s.key).unwrap();
        assert_eq!(g, m.wal_generation, "a segment of another generation");
        assert_eq!(
            start,
            wal.len() as u64,
            "segments must be contiguous from 0"
        );
        let bytes = get(store, &s.key).await.expect("a named segment exists");
        assert_eq!(bytes.len() as u64, end - start);
        assert_eq!(crc32_hex(&bytes), s.crc32);
        wal.extend(bytes);
    }
    std::fs::write(into.join("wal.bin"), wal).unwrap();
    let opened = GraphDb::open(into).expect("a restored store opens");
    assert_eq!(
        opened.wal_total_commits().unwrap(),
        m.commit_seq,
        "commit_seq matches what the manifest's files and segments hold"
    );
}

fn counts_of(dir: &Path) -> (usize, u64) {
    let db = GraphDb::open(dir).unwrap();
    (db.node_count(), db.edge_count())
}

fn counts(db: &SharedDb) -> (usize, u64) {
    let r = db.read();
    (r.node_count(), r.edge_count())
}

fn wal_frame(payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend((payload.len() as u32).to_le_bytes());
    f.extend(crc32fast::hash(payload).to_le_bytes());
    f.extend(payload);
    f
}

// ── The brief's five ─────────────────────────────────────────────────────────

#[test]
fn segment_excludes_partial_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal.bin");
    let frame = wal_frame(b"one complete frame");
    let mut bytes = frame.clone();
    bytes.extend(&wal_frame(b"the next one")[..3]);
    std::fs::write(&path, &bytes).unwrap();

    let (start, end, got) = read_new_segment(&path, 0, bytes.len() as u64)
        .unwrap()
        .expect("one whole frame to ship");
    assert_eq!((start, end), (0, frame.len() as u64));
    assert_eq!(got, frame, "the three partial bytes are not shipped");
    // From the boundary on there is only the partial frame: nothing to ship.
    assert!(read_new_segment(&path, end, bytes.len() as u64)
        .unwrap()
        .is_none());
}

#[test]
fn manifest_round_trip() {
    let m = Manifest {
        version: MANIFEST_VERSION,
        lineage: "0123abcd".into(),
        created_at: "2026-10-19T12:00:00Z".into(),
        commit_seq: 4712,
        oldest_commit_seq: 12,
        files: vec![FileEntry {
            name: "snapshot.bin".into(),
            key: "pfx/snapshots/12-4712.bin".into(),
            size: 18_432_000,
            crc32: "cbf43926".into(),
            frames: 0,
            lineage: "0123abcd".into(),
        }],
        wal_generation: 12,
        segments: vec![SegmentEntry {
            key: "pfx/wal/12/00000000000000000000-00000000000000065536".into(),
            crc32: "00000000".into(),
            frames: 17,
            lineage: "0123abcd".into(),
        }],
    };
    let json = m.to_json();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    for field in [
        "version",
        "lineage",
        "created_at",
        "commit_seq",
        "oldest_commit_seq",
        "files",
        "wal_generation",
        "segments",
    ] {
        assert!(v.get(field).is_some(), "{field} is in the JSON");
    }
    assert_eq!(Manifest::from_json(&json).unwrap(), m);
}

#[tokio::test(start_paused = true)]
async fn worker_uploads_segment_after_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let (_tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::spawn(
        config(1000),
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();

    // The first step runs at once: generation 0, everything durable so far.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let first = segment_keys(&store, 0).await;
    let shipped = first.last().expect("the first step ships the WAL").1;
    assert_eq!(shipped, db.durable_wal_position().len);

    db.write().insert_node("A", "b", vec![]).unwrap();
    let durable = db.durable_wal_position().len;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        segment_keys(&store, 0).await,
        first,
        "nothing ships before the interval elapses"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    let after = segment_keys(&store, 0).await;
    assert_eq!(after.len(), first.len() + 1);
    assert_eq!(*after.last().unwrap(), (shipped, durable));
    for k in keys(&store).await.iter().filter(|k| k.contains("/wal/")) {
        assert!(k.starts_with("pfx/wal/0/"), "{k}");
        let name = k.rsplit('/').next().unwrap();
        assert_eq!(name.len(), 41, "{{start:020}}-{{end:020}}: {k}");
    }
    assert_eq!(handle.error_count.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn worker_uploads_snapshot_on_trigger() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    // An hour between ticks: only the trigger can make the upload happen.
    let handle = BackupWorker::spawn(
        config(3_600_000),
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    let first = wait_for_latest(&store, |_| true).await;
    assert!(first.files.iter().all(|f| f.name != "snapshot.bin"));

    db.write().insert_node("A", "b", vec![]).unwrap();
    db.write().snapshot_with(ARCHIVING).unwrap();
    tx.send(BackupTrigger::Snapshot).unwrap();

    let m = wait_for_latest(&store, |m| m.wal_generation > first.wal_generation).await;
    let entry = m
        .files
        .iter()
        .find(|f| f.name == "snapshot.bin")
        .expect("the manifest names the snapshot");
    assert!(
        entry
            .key
            .starts_with(&format!("pfx/snapshots/{}-", m.wal_generation)),
        "a versioned key: {}",
        entry.key
    );
    let snap = get(&store, &entry.key).await.expect("snapshot uploaded");
    assert_eq!(
        snap,
        std::fs::read(dir.path().join("snapshot.bin")).unwrap()
    );
    assert_eq!(entry.crc32, crc32_hex(&snap));
    assert_eq!(handle.error_count.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn manifest_generation_increments_on_truncating_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let _handle = BackupWorker::spawn(
        config(3_600_000),
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    let base = wait_for_latest(&store, |_| true).await;

    db.write().insert_node("A", "b", vec![]).unwrap();
    db.write().snapshot().unwrap();
    tx.send(BackupTrigger::Snapshot).unwrap();
    let first = wait_for_latest(&store, |m| m.wal_generation > base.wal_generation).await;

    db.write().insert_node("A", "c", vec![]).unwrap();
    db.write().snapshot().unwrap();
    tx.send(BackupTrigger::Snapshot).unwrap();
    let second = wait_for_latest(&store, |m| m.wal_generation > first.wal_generation).await;

    assert_eq!(second.wal_generation, first.wal_generation + 1);
    // Each generation's segments restart at offset 0 under their own key.
    for g in [first.wal_generation, second.wal_generation] {
        let segs = segment_keys(&store, g).await;
        if let Some(s) = segs.first() {
            assert_eq!(s.0, 0, "generation {g} starts at 0");
        }
    }
}

/// Poll `LATEST` (real time) until `pred` holds.
async fn wait_for_latest(
    store: &Arc<dyn ObjectStore>,
    pred: impl Fn(&Manifest) -> bool,
) -> Manifest {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(m) = latest(store).await {
            if pred(&m) {
                return m;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "LATEST never reached the expected state"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ── Durability, restore round trips, continuity ──────────────────────────────

#[tokio::test]
async fn only_fsynced_bytes_are_shipped() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let mut w = worker(&store, &db, dir.path());
    w.run_once().await.unwrap();
    let shipped = segment_keys(&store, 0).await.last().unwrap().1;

    db.write().set_fsync_policy(FsyncPolicy::Relaxed);
    db.write().insert_node("A", "b", vec![]).unwrap();
    let file_len = std::fs::metadata(dir.path().join("wal.bin")).unwrap().len();
    assert!(file_len > shipped, "the relaxed write reached the file");
    w.run_once().await.unwrap();
    assert_eq!(
        segment_keys(&store, 0).await.last().unwrap().1,
        shipped,
        "bytes in the page cache only are not shipped"
    );

    // The next fsync covers them, and they ship.
    db.write().set_fsync_policy(FsyncPolicy::Strict);
    db.write().insert_node("A", "c", vec![]).unwrap();
    w.run_once().await.unwrap();
    let end = segment_keys(&store, 0).await.last().unwrap().1;
    assert_eq!(
        end,
        std::fs::metadata(dir.path().join("wal.bin")).unwrap().len()
    );
}

#[tokio::test]
async fn restore_round_trip_across_two_snapshot_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let store = mem();
    let mut w = worker(&store, &db, dir.path());
    let mut generations = Vec::new();
    let mut next = 0;
    let mut write = |db: &SharedDb, n: usize| {
        for _ in 0..n {
            let k = format!("n{next}");
            db.write().insert_node("A", &k, vec![]).unwrap();
            if next > 0 {
                db.write()
                    .insert_edge("E", &format!("n{}", next - 1), &k)
                    .unwrap();
            }
            next += 1;
        }
    };

    for round in 0..3 {
        write(&db, 20);
        // Several ticks inside one generation: several segments.
        w.run_once().await.unwrap();
        write(&db, 5);
        w.run_once().await.unwrap();
        // Manifests between snapshots are on a timer; force one here so the
        // restore sees the segments.
        w.flush().await;
        let restored = tempfile::tempdir().unwrap();
        let m = restore_latest(&store, restored.path()).await;
        assert!(!m.segments.is_empty(), "round {round}: segments shipped");
        assert_eq!(
            counts_of(restored.path()),
            counts(&db),
            "round {round}: restored store matches the source"
        );
        generations.push(m.wal_generation);
        if round < 2 {
            db.write().snapshot_with(ARCHIVING).unwrap();
            w.run_once().await.unwrap();
        }
    }
    generations.dedup();
    assert_eq!(
        generations.len(),
        3,
        "each snapshot boundary is a new generation"
    );
    let archives = keys(&store)
        .await
        .into_iter()
        .filter(|k| k.ends_with(".archive"))
        .count();
    assert_eq!(archives, 2, "both archives uploaded");
}

#[tokio::test]
async fn a_restart_starts_a_new_generation_with_no_gap() {
    let dir = tempfile::tempdir().unwrap();
    let store = mem();
    {
        let db = SharedDb::open(dir.path()).unwrap();
        db.write().insert_node("A", "a", vec![]).unwrap();
        let mut w = worker(&store, &db, dir.path());
        w.run_once().await.unwrap();
        // Written and fsynced, never shipped: the process dies here.
        db.write().insert_node("A", "b", vec![]).unwrap();
        db.write().insert_edge("E", "a", "b").unwrap();
    }
    let before = latest(&store).await.unwrap();

    let db = SharedDb::open(dir.path()).unwrap();
    let mut w = worker(&store, &db, dir.path());
    w.run_once().await.unwrap();
    let after = latest(&store).await.unwrap();
    assert!(
        after.wal_generation > before.wal_generation,
        "no generation reused"
    );
    let segs = segment_keys(&store, after.wal_generation).await;
    assert_eq!(segs.first().map(|s| s.0), Some(0));
    assert_eq!(
        segs.last().map(|s| s.1),
        Some(std::fs::metadata(dir.path().join("wal.bin")).unwrap().len()),
        "the restarted worker ships the whole WAL the old one had not"
    );
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), counts(&db));
    assert_eq!(counts(&db), (2, 1));
}

#[tokio::test]
async fn archives_are_uploaded_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let store = mem();
    let mut w = worker(&store, &db, dir.path());
    db.write().insert_node("A", "a", vec![]).unwrap();
    db.write().snapshot_with(ARCHIVING).unwrap();
    w.run_once().await.unwrap();
    let archive = keys(&store)
        .await
        .into_iter()
        .find(|k| k.ends_with(".archive"))
        .expect("the archive is uploaded");
    // Mark the uploaded copy; a re-upload would overwrite the mark.
    store
        .put(
            &ObjectPath::from(archive.as_str()),
            PutPayload::from(b"mark".to_vec()),
        )
        .await
        .unwrap();

    db.write().insert_node("A", "b", vec![]).unwrap();
    db.write().snapshot_with(ARCHIVING).unwrap();
    w.run_once().await.unwrap();
    assert_eq!(get(&store, &archive).await.unwrap(), b"mark");
    let n_archives = keys(&store)
        .await
        .iter()
        .filter(|k| k.ends_with(".archive"))
        .count();
    assert_eq!(n_archives, 2);
}

#[tokio::test]
async fn snapshot_intent_is_never_uploaded() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    db.write().snapshot().unwrap();
    assert!(dir.path().join("snapshot.intent").exists());
    let store = mem();
    let mut w = worker(&store, &db, dir.path());
    w.run_once().await.unwrap();
    assert!(keys(&store)
        .await
        .iter()
        .all(|k| !k.contains("snapshot.intent")));

    // A non-empty intent is a snapshot in progress — here or in another
    // process — or one stopped half-way: nothing is read beside it, and that
    // is a deferral, not a failure.
    std::fs::write(dir.path().join("snapshot.intent"), [1u8; 32]).unwrap();
    let fresh = mem();
    let mut w = worker(&fresh, &db, dir.path());
    let c = w.counters();
    w.step().await;
    assert_eq!(
        c.error_count.load(Ordering::Acquire),
        0,
        "a deferral is not an error"
    );
    assert!(
        c.healthy.load(Ordering::Acquire),
        "not unhealthy yet: no lag limit passed"
    );
    assert!(
        latest(&fresh).await.is_none(),
        "nothing was built beside the intent"
    );
}

#[tokio::test]
async fn shutdown_flushes_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let store = mem();
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::spawn(
        config(3_600_000),
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    wait_for_latest(&store, |_| true).await;
    for k in ["a", "b", "c"] {
        db.write().insert_node("A", k, vec![]).unwrap();
    }
    tx.send(BackupTrigger::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(20), handle.join)
        .await
        .expect("the worker stops")
        .unwrap();
    let m = latest(&store).await.unwrap();
    assert_eq!(m.commit_seq, db.read().wal_total_commits().unwrap());
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), (3, 0));
}

// ── Retention ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn retention_keeps_the_newest_manifests_and_what_they_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let store = mem();
    let mut w = BackupWorker::new(
        BackupConfig {
            retain_n: 2,
            ..config(1000)
        },
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    for i in 0..4 {
        db.write()
            .insert_node("A", &format!("n{i}"), vec![])
            .unwrap();
        db.write().snapshot_with(ARCHIVING).unwrap();
        w.run_once().await.unwrap();
    }
    let all = keys(&store).await;
    let manifests: Vec<&String> = all.iter().filter(|k| k.contains("/manifest/")).collect();
    assert_eq!(manifests.len(), 2, "{all:#?}");
    let latest_m = latest(&store).await.unwrap();
    let mut kept_generations = Vec::new();
    for k in &manifests {
        let m = Manifest::from_json(&get(&store, k).await.unwrap()).unwrap();
        for f in &m.files {
            assert!(get(&store, &f.key).await.is_some(), "{} survives", f.key);
        }
        for s in &m.segments {
            assert!(get(&store, &s.key).await.is_some(), "{} survives", s.key);
        }
        kept_generations.push(m.wal_generation);
    }
    let oldest_kept = *kept_generations.iter().min().unwrap();
    for k in &all {
        if let Some((g, _, _)) = parse_segment_key(PREFIX, k) {
            assert!(
                g >= oldest_kept,
                "segment {k} of a dropped generation survived"
            );
        }
    }
    let restored = tempfile::tempdir().unwrap();
    assert_eq!(restore_latest(&store, restored.path()).await, latest_m);
    assert_eq!(counts_of(restored.path()), counts(&db));
}

// ── Failures ─────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn failed_uploads_back_off_and_turn_health_off_then_on() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let mut w = worker(&store, &db, dir.path());
    let c = w.counters();

    // An unwritable bucket: every upload fails.
    std::fs::set_permissions(bucket.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut delays = Vec::new();
    for _ in 0..4 {
        delays.push(w.step().await);
    }
    assert_eq!(
        delays,
        [1, 2, 4, 8].map(Duration::from_secs),
        "exponential back-off"
    );
    assert_eq!(c.error_count.load(Ordering::Acquire), 4);
    assert!(
        !c.healthy.load(Ordering::Acquire),
        "unhealthy after 3 failures"
    );

    std::fs::set_permissions(bucket.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(w.step().await, Duration::from_millis(1000));
    assert!(c.healthy.load(Ordering::Acquire), "healthy again");
    assert!(
        c.last_success_ts.load(Ordering::Acquire) > 0,
        "last success recorded"
    );
    // Nothing was lost while the bucket was down.
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), (1, 0));
}

// ── Fix round 1 probes ───────────────────────────────────────────────────────

/// Every manifest in the bucket, oldest first.
async fn all_manifests(store: &Arc<dyn ObjectStore>) -> Vec<Manifest> {
    let mut out = Vec::new();
    for k in keys(store).await {
        if k.contains("/manifest/") {
            out.push(Manifest::from_json(&get(store, &k).await.unwrap()).unwrap());
        }
    }
    out
}

/// Every manifest in the bucket verifies and restores to a store holding
/// exactly its `commit_seq` frames; `LATEST`, when present, names one of them.
async fn assert_bucket_sound(store: &Arc<dyn ObjectStore>, context: &str) {
    let manifests = all_manifests(store).await;
    for m in &manifests {
        let into = tempfile::tempdir().unwrap();
        restore(store, m, into.path()).await;
    }
    if let Some(bytes) = get(store, &format!("{PREFIX}/LATEST")).await {
        let key = String::from_utf8(bytes).unwrap();
        assert!(
            get(store, &key).await.is_some(),
            "{context}: LATEST names {key}, which is gone"
        );
    }
}

fn prop(db: &SharedDb, key: &str, field: &str) -> Option<mushroomdb::Value> {
    db.read().get_prop(key, field)
}

/// Critical probe: a snapshot taken by another process — the CLI, while
/// `serve` runs — must not be paired with the previous WAL generation. The
/// server handle has not refreshed when the worker looks.
#[tokio::test]
async fn a_peer_process_snapshot_restores_to_the_live_store() {
    for archive in [false, true] {
        let options = SnapshotOptions {
            keep_wal: false,
            archive_wal: archive,
        };
        let dir = tempfile::tempdir().unwrap();
        let server = SharedDb::open(dir.path()).unwrap();
        server
            .write()
            .insert_node("A", "a1", vec![("v".into(), mushroomdb::Value::Int(1))])
            .unwrap();
        for i in 0..5 {
            server
                .write()
                .insert_node("A", &format!("s{i}"), vec![])
                .unwrap();
        }
        let store = mem();
        let mut w = worker(&store, &server, dir.path());
        w.run_once().await.unwrap();
        {
            let peer = SharedDb::open(dir.path()).unwrap();
            peer.write()
                .set_prop("a1", "v", mushroomdb::Value::Int(99))
                .unwrap();
            for i in 0..5 {
                peer.write()
                    .insert_node("B", &format!("p{i}"), vec![])
                    .unwrap();
            }
            peer.write().snapshot_with(options).unwrap();
        }
        // No read on the server in between: its handle is behind the disk.
        w.run_once().await.unwrap();
        w.flush().await;

        let restored = tempfile::tempdir().unwrap();
        let m = restore_latest(&store, restored.path()).await;
        let r = GraphDb::open(restored.path()).unwrap();
        let v = r.get_prop("a1", "v");
        drop(r);
        assert_eq!(v, Some(mushroomdb::Value::Int(99)), "archive={archive}");
        assert_eq!(prop(&server, "a1", "v"), Some(mushroomdb::Value::Int(99)));
        assert_eq!(
            counts_of(restored.path()),
            counts(&server),
            "archive={archive}"
        );
        assert_eq!(
            m.commit_seq,
            server.read().wal_total_commits().unwrap(),
            "archive={archive}: commit_seq is the live store's"
        );
        assert_bucket_sound(&store, "peer").await;
    }
}

/// The run every kill-point probe replays: writes, segment shipping, an
/// archiving and a truncating snapshot, retention at 2. Returns whether the
/// worker was "killed" (its write budget ran out) before the end.
async fn scenario(
    dir: &Path,
    store: &Arc<dyn ObjectStore>,
    budget: Option<u64>,
) -> (SharedDb, bool) {
    let db = SharedDb::open(dir).unwrap();
    let mut w = BackupWorker::new(
        BackupConfig {
            retain_n: 2,
            manifest_interval_s: 0,
            ..config(1000)
        },
        Arc::clone(store),
        Arc::new(db.clone()),
        dir.to_path_buf(),
    )
    .unwrap();
    if let Some(n) = budget {
        w.fail_after_writes(n);
    }
    let mut next = 0;
    for round in 0..4 {
        for _ in 0..3 {
            db.write()
                .insert_node("A", &format!("n{next}"), vec![])
                .unwrap();
            next += 1;
        }
        if w.run_once().await.is_err() {
            return (db, true);
        }
        match round {
            1 => db.write().snapshot_with(ARCHIVING).unwrap(),
            2 => db.write().snapshot().unwrap(),
            _ => {}
        }
        if w.run_once().await.is_err() {
            return (db, true);
        }
    }
    (db, false)
}

/// Important probe: a kill between any two requests that change the bucket.
/// After every one, every manifest left verifies and restores, and `LATEST`
/// names an existing manifest; a restarted worker then brings the bucket to
/// the live store.
#[tokio::test]
async fn a_kill_between_any_two_requests_leaves_every_manifest_restorable() {
    let mut kill_points = 0;
    for budget in 0.. {
        let dir = tempfile::tempdir().unwrap();
        let store = mem();
        let (db, killed) = scenario(dir.path(), &store, Some(budget)).await;
        if !killed {
            break;
        }
        kill_points += 1;
        assert_bucket_sound(&store, &format!("killed after {budget} writes")).await;
        drop(db);
        // The restart: a new process, a new worker, the same bucket.
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker(&store, &db, dir.path());
        w.run_once().await.unwrap();
        let restored = tempfile::tempdir().unwrap();
        restore_latest(&store, restored.path()).await;
        assert_eq!(
            counts_of(restored.path()),
            counts(&db),
            "after kill {budget}"
        );
        assert_bucket_sound(&store, &format!("restarted after kill {budget}")).await;
    }
    assert!(
        kill_points > 20,
        "the scenario makes many writes: {kill_points}"
    );
}

/// Important probe: side files change under the same name, and every
/// retained manifest must still verify after many cycles, not only the newest.
#[tokio::test]
async fn every_retained_manifest_verifies_after_many_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let store = mem();
    let (db, killed) = scenario(dir.path(), &store, None).await;
    assert!(!killed);
    let manifests = all_manifests(&store).await;
    assert_eq!(manifests.len(), 2, "retention keeps two");
    assert_bucket_sound(&store, "cycles").await;
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), counts(&db));
    // Every object a manifest names is immutable: no key appears under two
    // different checksums across manifests.
    let mut seen = std::collections::HashMap::new();
    for m in &manifests {
        for f in &m.files {
            let prev = seen.insert(f.key.clone(), f.crc32.clone());
            assert!(
                prev.is_none_or(|c| c == f.crc32),
                "{} was overwritten",
                f.key
            );
        }
    }
}

/// Important probe: a black-holed bucket. Every request hangs forever; the
/// worker must count failures, go unhealthy once durable data has waited
/// longer than 30 s, report the lag, and still stop within the shutdown grace.
#[tokio::test(start_paused = true)]
async fn a_hung_bucket_is_reported_and_does_not_hold_up_shutdown() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let forever = Duration::from_secs(365 * 24 * 3600);
    let hung: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: forever,
            wait_get_per_call: forever,
            wait_list_per_call: forever,
            wait_list_with_delimiter_per_call: forever,
            wait_delete_per_call: forever,
            ..ThrottleConfig::default()
        },
    ));
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::spawn(
        BackupConfig {
            op_timeout_ms: 200,
            shutdown_grace_ms: 500,
            ..config(1000)
        },
        hung,
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        handle.error_count.load(Ordering::Acquire) > 0,
        "timeouts count as failures"
    );

    tokio::time::sleep(Duration::from_secs(40)).await;
    assert!(
        !handle.healthy.load(Ordering::Acquire),
        "unhealthy after 30 s of lag"
    );
    assert!(
        handle.lag_s.load(Ordering::Acquire) >= 30,
        "lag is reported"
    );

    let asked = tokio::time::Instant::now();
    tx.send(BackupTrigger::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.join)
        .await
        .expect("shutdown is bounded however hung the bucket is")
        .unwrap();
    assert!(
        asked.elapsed() <= Duration::from_millis(1000),
        "{:?}",
        asked.elapsed()
    );
}

/// Lag and health come back once the bucket answers again.
#[tokio::test]
async fn lag_is_zero_when_everything_durable_has_shipped() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let mut w = worker(&store, &db, dir.path());
    let c = w.counters();
    w.step().await;
    assert_eq!(c.lag_s.load(Ordering::Acquire), 0);
    assert!(c.healthy.load(Ordering::Acquire));
    assert!(c.last_success_ts.load(Ordering::Acquire) > 0);
}

// ── Fix round 2 probes ───────────────────────────────────────────────────────

/// Critical probe: another process runs a storm of truncating, archiving and
/// keep-WAL snapshots, with writes between them, while the worker steps and
/// the server keeps reading (each read may refresh mid-snapshot). Every
/// manifest must verify, its `commit_seq` must be what the restored store
/// holds, and where the live store can still be opened at that commit the
/// restored store must equal it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_storm_from_another_process_leaves_every_manifest_true() {
    let dir = tempfile::tempdir().unwrap();
    let server = SharedDb::open(dir.path()).unwrap();
    for i in 0..5 {
        server
            .write()
            .insert_node("S", &format!("s{i}"), vec![])
            .unwrap();
    }
    let store = mem();
    let mut w = BackupWorker::new(
        BackupConfig {
            retain_n: 0,
            manifest_interval_s: 0,
            ..config(1000)
        },
        Arc::clone(&store),
        Arc::new(server.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    w.run_once().await.unwrap();

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let peer = {
        let dir = dir.path().to_path_buf();
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let peer = SharedDb::open(&dir).unwrap();
            // 30 mixed snapshots; then one truncating snapshot and writes
            // only, so the history from there on is reachable through
            // `open_at` (a keep-WAL snapshot is no base for it).
            for i in 0..36i64 {
                for j in 0..3 {
                    peer.write()
                        .insert_node(
                            "P",
                            &format!("p{i}-{j}"),
                            vec![("i".into(), mushroomdb::Value::Int(i))],
                        )
                        .unwrap();
                }
                if i > 0 {
                    peer.write()
                        .insert_edge("E", &format!("p{}-0", i - 1), &format!("p{i}-0"))
                        .unwrap();
                }
                let opts = match i % 3 {
                    _ if i > 30 => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    _ if i == 30 => SnapshotOptions::default(),
                    0 => SnapshotOptions::default(),
                    1 => ARCHIVING,
                    _ => SnapshotOptions {
                        keep_wal: true,
                        archive_wal: false,
                    },
                };
                peer.write().snapshot_with(opts).unwrap();
                std::thread::sleep(Duration::from_millis(3));
            }
            done.store(true, Ordering::Release);
        })
    };
    let mut steps = 0;
    while !done.load(Ordering::Acquire) {
        w.run_once()
            .await
            .expect("a storm defers steps, it never fails them");
        let _ = server.read().node_count();
        steps += 1;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    peer.join().unwrap();
    for _ in 0..3 {
        w.run_once().await.unwrap();
    }
    w.flush().await;

    let manifests = all_manifests(&store).await;
    assert!(
        manifests.len() >= 10,
        "{} manifests in {steps} steps",
        manifests.len()
    );
    let mut compared = 0;
    for m in &manifests {
        let into = tempfile::tempdir().unwrap();
        // Asserts every checksum, contiguity, and commit_seq == the restored
        // store's frame count.
        restore(&store, m, into.path()).await;
        if m.commit_seq == 0 {
            continue;
        }
        if let Ok(at) = GraphDb::open_at(dir.path(), m.commit_seq - 1) {
            let live_then = (at.node_count(), at.edge_count());
            drop(at);
            assert_eq!(
                counts_of(into.path()),
                live_then,
                "manifest at {} restores what the store held then",
                m.commit_seq
            );
            compared += 1;
        }
    }
    if compared == 0 {
        let total = counts(&server);
        let last = server.read().wal_total_commits().unwrap();
        panic!(
            "no manifest could be compared with open_at; at the end: {:?}",
            GraphDb::open_at(dir.path(), last - 1).map(|d| (d.node_count(), total))
        );
    }
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), counts(&server));
}

/// A store whose next upload is a snapshot of its own, with nothing before it:
/// one truncating snapshot (no archive, so no chain files) and a write.
fn store_with_a_snapshot(dir: &Path) -> SharedDb {
    let db = SharedDb::open(dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    db.write().snapshot().unwrap();
    db.write().insert_node("A", "b", vec![]).unwrap();
    db
}

/// Important probe: a refused snapshot PUT (the bucket answers it with an
/// error) must be counted and retried, never panic the worker.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_snapshot_put_is_counted_and_retried() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let db = store_with_a_snapshot(dir.path());
    let refused = bucket.path().join(PREFIX).join("snapshots");
    std::fs::create_dir_all(&refused).unwrap();
    std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o500)).unwrap();
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::new(
        BackupConfig {
            wal_interval_ms: 50,
            ..config(50)
        },
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap()
    .start(rx);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while handle.error_count.load(Ordering::Acquire) < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no failure counted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!handle.join.is_finished(), "the worker is still running");
    assert!(
        handle
            .reason
            .lock()
            .unwrap()
            .as_deref()
            .is_none_or(|r| !r.contains("panicked")),
        "no panic"
    );
    // The bucket comes back: the retry succeeds and the backup restores.
    std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o700)).unwrap();
    wait_for_latest(&store, |m| m.files.iter().any(|f| f.name == "snapshot.bin")).await;
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    let restored = tempfile::tempdir().unwrap();
    restore_latest(&store, restored.path()).await;
    assert_eq!(counts_of(restored.path()), counts(&db));
}

/// Important probe: a snapshot PUT that hangs, with `snapshot.bin` present,
/// times out and is counted; the worker keeps going and shuts down on time.
#[tokio::test(start_paused = true)]
async fn a_hung_snapshot_put_times_out_without_killing_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with_a_snapshot(dir.path());
    // Every upload but the lease hangs: the worker holds its lease and the
    // snapshot PUT is what times out.
    let faulty = support::Faulty::new(mem());
    faulty
        .hang_data_puts
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let hung: Arc<dyn ObjectStore> = Arc::clone(&faulty) as Arc<dyn ObjectStore>;
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::spawn(
        BackupConfig {
            op_timeout_ms: 200,
            shutdown_grace_ms: 500,
            ..config(1000)
        },
        hung,
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(
        handle.error_count.load(Ordering::Acquire) >= 3,
        "each timeout is counted"
    );
    assert!(
        !handle.join.is_finished(),
        "the worker survived every failed PUT"
    );
    assert!(
        !handle.healthy.load(Ordering::Acquire),
        "three failures in a row"
    );
    let reason = handle.reason.lock().unwrap().clone().unwrap();
    assert!(reason.contains("consecutive failed"), "{reason}");
    assert!(
        faulty.data_writes() >= 3,
        "the snapshot PUT was attempted and timed out each time"
    );
    tx.send(BackupTrigger::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.join)
        .await
        .expect("shutdown is bounded")
        .unwrap();
}

/// Important probe: if the worker ever dies — here, a forced panic — health
/// says so, with the reason, and stays that way.
#[tokio::test]
async fn a_worker_that_panics_reports_unhealthy_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let mut w = worker(&mem(), &db, dir.path());
    w.panic_on_next_step();
    let (_tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = w.start(rx);
    let healthy = Arc::clone(&handle.healthy);
    let reason = Arc::clone(&handle.reason);
    tokio::time::timeout(Duration::from_secs(10), handle.join)
        .await
        .expect("the supervisor notices the worker's end")
        .unwrap();
    assert!(!healthy.load(Ordering::Acquire));
    let why = reason.lock().unwrap().clone().unwrap();
    assert!(
        why.contains("panicked") && why.contains("test hook"),
        "{why}"
    );
}

/// Minor probe: health flips within one cadence of the 30 s limit, and
/// `lag_s` counts from when the unshipped durable data was first seen.
#[tokio::test(start_paused = true)]
async fn health_flips_within_one_cadence_of_the_lag_limit() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let forever = Duration::from_secs(365 * 24 * 3600);
    // Every request hangs, and the deadline is longer than the test: no
    // failure is ever counted, so only the lag can make it unhealthy.
    let hung: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_put_per_call: forever,
            wait_get_per_call: forever,
            wait_list_with_delimiter_per_call: forever,
            ..ThrottleConfig::default()
        },
    ));
    let (_tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = BackupWorker::spawn(
        BackupConfig {
            op_timeout_ms: 3_600_000,
            ..config(1000)
        },
        hung,
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
        rx,
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(29_900)).await;
    assert!(
        handle.healthy.load(Ordering::Acquire),
        "29.9 s: within the limit"
    );
    assert_eq!(handle.lag_s.load(Ordering::Acquire), 29);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        handle.healthy.load(Ordering::Acquire),
        "30.5 s: the limit itself"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !handle.healthy.load(Ordering::Acquire),
        "31.1 s: past it by one cadence at most"
    );
    assert_eq!(handle.lag_s.load(Ordering::Acquire), 31);
    assert_eq!(handle.error_count.load(Ordering::Acquire), 0);
    let reason = handle.reason.lock().unwrap().clone().unwrap();
    assert!(reason.contains("unshipped for 31s"), "{reason}");
}

/// Minor probe: retention's cost does not grow with the number of segments
/// in a generation that never snapshots.
#[tokio::test]
async fn retention_cost_does_not_grow_with_the_segments_of_a_generation() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let store = mem();
    let mut w = BackupWorker::new(
        BackupConfig {
            retain_n: 2,
            manifest_interval_s: 0,
            ..config(1000)
        },
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let mut per_cycle = Vec::new();
    for i in 0..40 {
        db.write()
            .insert_node("A", &format!("n{i}"), vec![])
            .unwrap();
        let before = w.list_requests();
        w.run_once().await.unwrap();
        per_cycle.push(w.list_requests() - before);
    }
    let segments = segment_keys(&store, 0).await.len();
    assert!(segments >= 40, "{segments} segments in one generation");
    assert_eq!(
        per_cycle[10], per_cycle[39],
        "LISTs per manifest write stay flat: {per_cycle:?}"
    );
    assert_bucket_sound(&store, "flat retention").await;
}
