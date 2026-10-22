//! `mushroomdb restore --from s3://…` (HO-09) against an in-memory bucket and
//! a bucket on local disk, filled by a real `BackupWorker` from a real store.
//!
//! Every restore here is compared with the source: node and edge counts and
//! the commit count, and for a point in time the whole graph against
//! `open_at` on the source at that commit (Review Focus 1).
//!
//! Every restore gives the restored store a new backup lineage, so its next
//! backup can never resume into, or delete from, the prefix it came from
//! (defect-ledger row 138).

use backup::manifest::{crc32_hex, latest_key, manifest_key, object_key, side_key};
use backup::restore::{restore_from_store, RestoreS3Error, RestoreTarget};
use backup::{BackupConfig, BackupError, BackupWorker, FileEntry, Manifest};
use mushroomdb::{GraphDb, RealFs, SharedDb, SnapshotOptions};
use object_store::local::LocalFileSystem;
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

const TRUNCATING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: false,
};

/// A bucket, and the directory behind it when it is on disk.
struct Bucket {
    store: Arc<dyn ObjectStore>,
    _dir: Option<tempfile::TempDir>,
}

fn in_memory() -> Bucket {
    Bucket {
        store: Arc::new(InMemory::new()),
        _dir: None,
    }
}

fn on_disk() -> Bucket {
    let dir = tempfile::tempdir().unwrap();
    Bucket {
        store: Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap()),
        _dir: Some(dir),
    }
}

/// Both backends every restore test runs against.
fn buckets() -> [(&'static str, Bucket); 2] {
    [("memory", in_memory()), ("disk", on_disk())]
}

fn config() -> BackupConfig {
    BackupConfig {
        manifest_interval_s: 0,
        retain_n: 0,
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

fn insert(db: &SharedDb, prefix: &str, n: usize) {
    for i in 0..n {
        db.write()
            .insert_node("A", &format!("{prefix}{i}"), vec![])
            .unwrap();
    }
}

fn link(db: &SharedDb, a: &str, b: &str) {
    db.write().insert_edge("L", a, b).unwrap();
}

async fn get(store: &Arc<dyn ObjectStore>, key: &str) -> Option<Vec<u8>> {
    match store.get(&ObjectPath::from(key)).await {
        Ok(r) => Some(r.bytes().await.unwrap().to_vec()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(e) => panic!("get {key}: {e}"),
    }
}

async fn put(store: &Arc<dyn ObjectStore>, key: &str, bytes: Vec<u8>) {
    store
        .put(&ObjectPath::from(key), PutPayload::from(bytes))
        .await
        .unwrap();
}

async fn latest(store: &Arc<dyn ObjectStore>) -> Manifest {
    let key = String::from_utf8(get(store, &latest_key(PREFIX)).await.unwrap()).unwrap();
    Manifest::from_json(&get(store, &key).await.unwrap()).unwrap()
}

/// Every key under the prefix with its bytes, sorted.
async fn bucket_contents(store: &Arc<dyn ObjectStore>) -> Vec<(String, Vec<u8>)> {
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

fn counts_of(dir: &Path) -> (usize, u64) {
    let db = GraphDb::open(dir).unwrap();
    (db.node_count(), db.edge_count())
}

fn counts(db: &SharedDb) -> (usize, u64) {
    let r = db.read();
    (r.node_count(), r.edge_count())
}

/// The whole graph. (Not the commit count: an `open_at` view reports the
/// store's, not its own.)
type Dump = (Vec<mushroomdb::NodeInfo>, Vec<mushroomdb::ExportEdge>);

fn dump(db: &GraphDb<RealFs>) -> Dump {
    (db.all_nodes_for_export(), db.all_edges_for_export())
}

fn lineage_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("backup.lineage"))
        .unwrap()
        .trim()
        .to_string()
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A source store backed up across two generations: commits 0..50 in the
/// first, an archiving snapshot, then commits 50..100 in the second, with a
/// few edges in each. Returns the store and its directory; the
/// first manifest is at commit 50 and the second at 100.
async fn two_generations(store: &Arc<dyn ObjectStore>) -> (SharedDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut w = worker(store, &db, dir.path());
    insert(&db, "a", 48);
    link(&db, "a0", "a1");
    link(&db, "a2", "a3");
    w.run_once().await.unwrap();
    let first = latest(store).await;
    assert_eq!(first.commit_seq, 50);
    db.write().snapshot_with(ARCHIVING).unwrap();
    insert(&db, "b", 48);
    link(&db, "b0", "a0");
    link(&db, "b1", "b2");
    w.run_once().await.unwrap();
    let second = latest(store).await;
    assert!(second.commit_seq >= 100, "{}", second.commit_seq);
    assert_ne!(
        first.wal_generation, second.wal_generation,
        "both generations are in the bucket"
    );
    (db, dir)
}

// ── The brief's four ─────────────────────────────────────────────────────────

/// The latest restore point, downloaded with every checksum verified, opens
/// to the source: the same nodes, edges and commit count, from every
/// segment the manifest names.
#[tokio::test]
async fn restore_downloads_manifest_and_verifies_sha256() {
    for (backend, b) in buckets() {
        let (db, _dir) = two_generations(&b.store).await;
        let m = latest(&b.store).await;
        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let out = restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
            .await
            .unwrap_or_else(|e| panic!("{backend}: {e}"));
        assert_eq!(out.commits, m.commit_seq, "{backend}");
        assert_eq!(out.segments, m.segments.len(), "{backend}");
        assert!(out.segments > 0, "{backend}");
        assert_eq!(out.manifest, manifest_key(PREFIX, m.commit_seq));
        assert!(
            !target.join("snapshot.intent").exists() && !target.join("LOCK").exists(),
            "{backend}: {:?}",
            dir_entries(&target)
        );
        for f in &m.files {
            let bytes = std::fs::read(target.join(&f.name)).unwrap();
            if f.name == "backup.lineage" {
                continue;
            }
            assert_eq!(crc32_hex(&bytes), f.crc32, "{backend} {}", f.name);
        }
        assert_eq!(counts_of(&target), counts(&db), "{backend}");
        assert_eq!(
            GraphDb::open(&target).unwrap().wal_total_commits().unwrap(),
            db.read().wal_total_commits().unwrap(),
            "{backend}"
        );
    }
}

/// `--at 75` restores the store as it was right after commit 75: the second
/// generation's restore point, cut after that commit's frame, so nothing
/// after it is in the restored store.
#[tokio::test]
async fn restore_pitr_excludes_later_segments() {
    for (backend, b) in buckets() {
        let (_db, dir) = two_generations(&b.store).await;
        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let out = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(75)),
            &target,
        )
        .await
        .unwrap_or_else(|e| panic!("{backend}: {e}"));
        assert_eq!(out.commits, 76, "{backend}");
        let restored = GraphDb::open(&target).unwrap();
        assert_eq!(restored.wal_total_commits().unwrap(), 76, "{backend}");
        let want = GraphDb::open_at(dir.path(), 75).unwrap();
        assert_eq!(dump(&restored), dump(&want), "{backend}");
        assert!(
            restored.has_node("b25") && !restored.has_node("b26"),
            "{backend}"
        );
    }
}

/// A point in time below the oldest retained commit is a clean error, and
/// nothing is written.
#[tokio::test]
async fn restore_pitr_before_oldest_archive_errors() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        insert(&db, "a", 50);
        // A truncating snapshot discards commits 0..50: the floor is 50.
        db.write().snapshot_with(TRUNCATING).unwrap();
        insert(&db, "b", 10);
        worker(&b.store, &db, dir.path()).run_once().await.unwrap();
        assert_eq!(latest(&b.store).await.oldest_commit_seq, 50);

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let e = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(5)),
            &target,
        )
        .await
        .unwrap_err();
        match e {
            RestoreS3Error::BeforeOldestArchive { target, oldest } => {
                assert_eq!((target, oldest), (5, 50), "{backend}")
            }
            other => panic!("{backend}: expected BeforeOldestArchive, got {other}"),
        }
        assert!(!target.exists(), "{backend}");
        assert!(
            dir_entries(into.path()).is_empty(),
            "{backend}: no staging left"
        );
        // Commit 55 is past the floor and restores.
        restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(55)),
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            GraphDb::open(&target).unwrap().wal_total_commits().unwrap(),
            56
        );
    }
}

/// A timestamp needs `commit_times.bin`; a backup without one says so.
#[tokio::test]
async fn restore_timestamp_pitr_without_commit_times_errors() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = SharedDb::open(dir.path()).unwrap();
            insert(&db, "a", 5);
        }
        std::fs::remove_file(dir.path().join("commit_times.bin")).unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        worker(&b.store, &db, dir.path()).run_once().await.unwrap();
        assert!(!latest(&b.store)
            .await
            .files
            .iter()
            .any(|f| f.name == "commit_times.bin"));

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let at = RestoreTarget::parse("2026-01-01T00:00:00Z").unwrap();
        let e = restore_from_store(Arc::clone(&b.store), PREFIX, Some(at), &target)
            .await
            .unwrap_err();
        assert!(
            matches!(e, RestoreS3Error::CommitTimesAbsent),
            "{backend}: {e}"
        );
        assert!(!target.exists(), "{backend}");
    }
}

// ── Point in time by timestamp ───────────────────────────────────────────────

/// A timestamp resolves through the backup's `commit_times.bin` to the last
/// commit at or before it, as a date query would.
#[tokio::test]
async fn restore_at_a_timestamp_restores_the_last_commit_at_or_before_it() {
    let b = in_memory();
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let t = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
    };
    let ms = |s: &str| (t(s).unix_timestamp_nanos() / 1_000_000) as i64;
    db.write()
        .record_commits_at(Some(ms("2026-01-01T00:00:00Z")))
        .unwrap();
    insert(&db, "jan", 3);
    db.write()
        .record_commits_at(Some(ms("2026-02-01T00:00:00Z")))
        .unwrap();
    insert(&db, "feb", 3);
    worker(&b.store, &db, dir.path()).run_once().await.unwrap();

    let into = tempfile::tempdir().unwrap();
    let target = into.path().join("restored");
    let out = restore_from_store(
        Arc::clone(&b.store),
        PREFIX,
        Some(RestoreTarget::Timestamp(t("2026-01-15T00:00:00Z"))),
        &target,
    )
    .await
    .unwrap();
    assert_eq!(out.commits, 3);
    let restored = GraphDb::open(&target).unwrap();
    assert!(restored.has_node("jan2") && !restored.has_node("feb0"));

    let before = into.path().join("before");
    let e = restore_from_store(
        Arc::clone(&b.store),
        PREFIX,
        Some(RestoreTarget::Timestamp(t("2025-12-31T00:00:00Z"))),
        &before,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, RestoreS3Error::Unreachable(_)), "{e}");
    assert!(!before.exists());
}

#[test]
fn a_target_parses_as_a_commit_or_an_rfc3339_timestamp() {
    assert_eq!(
        RestoreTarget::parse("75").unwrap(),
        RestoreTarget::CommitSeq(75)
    );
    assert!(matches!(
        RestoreTarget::parse("2026-10-01T12:00:00Z").unwrap(),
        RestoreTarget::Timestamp(_)
    ));
    for bad in ["", "-1", "yesterday", "2026-13-01T00:00:00Z"] {
        assert!(RestoreTarget::parse(bad).is_err(), "{bad:?}");
    }
}

// ── Row 138: every restore is a new lineage ──────────────────────────────────

/// Every kind of restore — the latest restore point, an older generation by
/// commit, and a point in time by timestamp — gives the restored store a new
/// lineage. Grown past the source's newest restore point and backed up to
/// the same prefix, it neither resumes into the source lineage nor deletes
/// anything of it: the bucket is byte-for-byte what it was.
#[tokio::test]
async fn every_restore_is_a_new_lineage_that_never_resumes_into_the_source() {
    for (backend, b) in buckets() {
        let (db, dir) = two_generations(&b.store).await;
        let source = lineage_of(dir.path());
        let newest = latest(&b.store).await.commit_seq;
        let first_generation_time = {
            let r = db.read();
            r.commit_time_ms(10).unwrap()
        };
        let at_time = time::OffsetDateTime::from_unix_timestamp_nanos(
            first_generation_time as i128 * 1_000_000,
        )
        .unwrap();
        drop(db);

        for (what, at) in [
            ("latest", None),
            ("an older generation", Some(RestoreTarget::CommitSeq(30))),
            ("a point in time", Some(RestoreTarget::Timestamp(at_time))),
        ] {
            let into = tempfile::tempdir().unwrap();
            let target = into.path().join("restored");
            let out = restore_from_store(Arc::clone(&b.store), PREFIX, at, &target)
                .await
                .unwrap_or_else(|e| panic!("{backend}, {what}: {e}"));
            let lineage = lineage_of(&target);
            assert_ne!(lineage, source, "{backend}, {what}: a new lineage");
            assert_eq!(out.lineage, lineage);
            assert_eq!(out.source_lineage, source);

            // Grow past the source's newest restore point, so the rewound
            // check alone would let it through, then back up to the prefix.
            let restored = SharedDb::open(&target).unwrap();
            insert(&restored, "grown", (newest + 5) as usize);
            assert!(restored.read().wal_total_commits().unwrap() > newest);
            let before = bucket_contents(&b.store).await;
            let mut w = worker_with(
                &b.store,
                &restored,
                &target,
                BackupConfig {
                    retain_n: 1,
                    ..config()
                },
            );
            let c = w.counters();
            match w.run_once().await.unwrap_err() {
                BackupError::Lineage(m) => assert!(m.contains("another store"), "{m}"),
                other => panic!("{backend}, {what}: expected a lineage refusal, got {other}"),
            }
            w.step().await;
            assert!(!c.healthy.load(Ordering::Acquire), "{backend}, {what}");
            assert_eq!(
                bucket_contents(&b.store).await,
                before,
                "{backend}, {what}: the source lineage is untouched"
            );
            assert_eq!(lineage_of(&target), lineage, "{backend}, {what}");

            // The same store backs up to a new prefix under its new lineage.
            let fresh = in_memory();
            worker(&fresh.store, &restored, &target)
                .run_once()
                .await
                .unwrap();
            assert_eq!(latest(&fresh.store).await.lineage, lineage);
        }
    }
}

/// A restore into a directory that already holds anything is refused before
/// anything is downloaded, and the directory is left as it was.
#[tokio::test]
async fn restore_into_a_non_empty_directory_is_refused() {
    let b = in_memory();
    let (_db, _dir) = two_generations(&b.store).await;
    let into = tempfile::tempdir().unwrap();
    std::fs::write(into.path().join("keep.txt"), b"mine").unwrap();
    let e = restore_from_store(Arc::clone(&b.store), PREFIX, None, into.path())
        .await
        .unwrap_err();
    assert!(matches!(e, RestoreS3Error::IntoNotEmpty { .. }), "{e}");
    assert_eq!(dir_entries(into.path()), ["keep.txt"]);

    // An empty directory that exists is fine, and is filled in place.
    let empty = tempfile::tempdir().unwrap();
    restore_from_store(Arc::clone(&b.store), PREFIX, None, empty.path())
        .await
        .unwrap();
    assert!(empty.path().join("wal.bin").exists());
}

/// A restore point that fails verification — an object whose bytes differ
/// from its checksum, or a lineage file that names another lineage — writes
/// nothing to the target: a missing target stays missing, an empty one stays
/// empty, and no staging directory is left beside it.
#[tokio::test]
async fn a_restore_point_that_fails_verification_leaves_the_target_untouched() {
    for (backend, b) in buckets() {
        let (_db, _dir) = two_generations(&b.store).await;
        let m = latest(&b.store).await;

        // A segment whose bytes no longer match the manifest.
        let seg = &m.segments[0].key;
        let good = get(&b.store, seg).await.unwrap();
        let mut bad = good.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        put(&b.store, seg, bad).await;
        let parent = tempfile::tempdir().unwrap();
        // Two directories that do not exist yet above the target: neither is
        // left behind either.
        let missing = parent.path().join("deep").join("er").join("restored");
        let e = restore_from_store(Arc::clone(&b.store), PREFIX, None, &missing)
            .await
            .unwrap_err();
        assert!(matches!(e, RestoreS3Error::Verify(_)), "{backend}: {e}");
        assert!(e.to_string().contains(seg.as_str()), "{backend}: {e}");
        assert!(!missing.exists(), "{backend}");
        assert!(
            dir_entries(parent.path()).is_empty(),
            "{backend}: no staging left"
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(
            restore_from_store(Arc::clone(&b.store), PREFIX, None, empty.path())
                .await
                .is_err()
        );
        assert!(dir_entries(empty.path()).is_empty(), "{backend}");
        put(&b.store, seg, good).await;

        // A lineage file naming another lineage than its manifest: a
        // manifest that lists it, under a fresh key, checksums and all.
        let foreign = b"0123456789abcdef0123456789abcdef".to_vec();
        let crc = crc32_hex(&foreign);
        let key = side_key(PREFIX, "backup.lineage", &crc, foreign.len() as u64);
        put(&b.store, &key, foreign.clone()).await;
        let mut forged = m.clone();
        for f in &mut forged.files {
            if f.name == "backup.lineage" {
                f.key = key.clone();
                f.crc32 = crc.clone();
                f.size = foreign.len() as u64;
            }
        }
        forged.commit_seq += 1;
        let forged_key = manifest_key(PREFIX, forged.commit_seq);
        put(&b.store, &forged_key, forged.to_json()).await;
        put(&b.store, &latest_key(PREFIX), forged_key.into_bytes()).await;
        let e = restore_from_store(Arc::clone(&b.store), PREFIX, None, &missing)
            .await
            .unwrap_err();
        assert!(matches!(e, RestoreS3Error::Verify(_)), "{backend}: {e}");
        assert!(e.to_string().contains("lineage"), "{backend}: {e}");
        assert!(!missing.exists(), "{backend}");
        assert!(dir_entries(parent.path()).is_empty(), "{backend}");
    }
}

/// A point in time is never assembled from another lineage's manifest, even
/// one at a closer commit count.
#[tokio::test]
async fn a_point_in_time_uses_only_the_latest_lineage() {
    let b = in_memory();
    let (_db, dir) = two_generations(&b.store).await;
    let mut other = latest(&b.store).await;
    other.lineage = "ffffffffffffffffffffffffffffffff".into();
    for f in &mut other.files {
        f.lineage = other.lineage.clone();
    }
    for s in &mut other.segments {
        s.lineage = other.lineage.clone();
    }
    other.commit_seq = 31;
    put(&b.store, &manifest_key(PREFIX, 31), other.to_json()).await;

    let into = tempfile::tempdir().unwrap();
    let target = into.path().join("restored");
    let out = restore_from_store(
        Arc::clone(&b.store),
        PREFIX,
        Some(RestoreTarget::CommitSeq(30)),
        &target,
    )
    .await
    .unwrap();
    assert_eq!(out.manifest, manifest_key(PREFIX, 50));
    assert_eq!(
        dump(&GraphDb::open(&target).unwrap()),
        dump(&GraphDb::open_at(dir.path(), 30).unwrap())
    );
}

/// Segments shipped after the newest manifest are part of a restore of the
/// latest, up to the last contiguous one: the recovery point is the segment
/// cadence, not the manifest interval.
#[tokio::test]
async fn segments_after_the_newest_manifest_are_restored_too() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker_with(
            &b.store,
            &db,
            dir.path(),
            BackupConfig {
                manifest_interval_s: 3600,
                ..config()
            },
        );
        insert(&db, "a", 5);
        w.run_once().await.unwrap();
        assert_eq!(latest(&b.store).await.commit_seq, 5);
        insert(&db, "b", 4);
        w.run_once().await.unwrap();
        insert(&db, "c", 3);
        w.run_once().await.unwrap();
        assert_eq!(latest(&b.store).await.commit_seq, 5, "no manifest was due");

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let out = restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
            .await
            .unwrap();
        assert_eq!(out.commits, 12, "{backend}");
        assert_eq!(counts_of(&target), counts(&db), "{backend}");
        // A point in time among them is reachable too.
        let at = into.path().join("at");
        let out = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(7)),
            &at,
        )
        .await
        .unwrap();
        assert_eq!(out.commits, 8, "{backend}");
        let e = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(12)),
            &into.path().join("past"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                e,
                RestoreS3Error::AfterLatest {
                    target: 12,
                    commits: 12,
                    not_restorable: None,
                }
            ),
            "{backend}: {e}"
        );
    }
}

/// Every file the manifest names is restored under its own name, whatever it
/// is — the snapshot byte for byte, so a store restores in the format it was
/// backed up in, and a side file this release does not itself upload (as a
/// format's own marker file would be).
#[tokio::test]
async fn every_named_file_restores_byte_for_byte() {
    let b = in_memory();
    let (_db, dir) = two_generations(&b.store).await;
    let mut m = latest(&b.store).await;
    let marker = b"marker".to_vec();
    let crc = crc32_hex(&marker);
    let key = side_key(PREFIX, "wal.genesis.v11", &crc, marker.len() as u64);
    put(&b.store, &key, marker.clone()).await;
    m.files.push(FileEntry {
        name: "wal.genesis.v11".into(),
        key,
        size: marker.len() as u64,
        crc32: crc,
        frames: 0,
        lineage: m.lineage.clone(),
    });
    let key = object_key(PREFIX, "manifest/marker.json");
    put(&b.store, &key, m.to_json()).await;
    put(&b.store, &latest_key(PREFIX), key.into_bytes()).await;

    let into = tempfile::tempdir().unwrap();
    let target = into.path().join("restored");
    restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(target.join("wal.genesis.v11")).unwrap(),
        marker
    );
    assert_eq!(
        std::fs::read(target.join("snapshot.bin")).unwrap(),
        std::fs::read(dir.path().join("snapshot.bin")).unwrap(),
        "the snapshot is the source's, byte for byte"
    );
    assert_eq!(
        mushroomdb::snapshot_version_at(&target).unwrap(),
        mushroomdb::snapshot_version_at(dir.path()).unwrap()
    );
}

#[tokio::test]
async fn an_empty_prefix_is_no_backup() {
    let b = in_memory();
    let into = tempfile::tempdir().unwrap();
    let target = into.path().join("restored");
    let e = restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
        .await
        .unwrap_err();
    assert!(matches!(e, RestoreS3Error::NoBackup { .. }), "{e}");
    assert!(!target.exists());
}

/// A commit above the floor whose history a truncating snapshot broke — it
/// sits in an archive that does not start at the store's first commit — is
/// out of reach, as it is for `asof`, and the target stays untouched.
#[tokio::test]
async fn a_commit_whose_history_is_not_whole_is_unreachable() {
    let b = in_memory();
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    insert(&db, "a", 5);
    db.write().snapshot_with(TRUNCATING).unwrap();
    insert(&db, "b", 5);
    db.write().snapshot_with(ARCHIVING).unwrap();
    insert(&db, "c", 5);
    worker(&b.store, &db, dir.path()).run_once().await.unwrap();
    let m = latest(&b.store).await;
    assert_eq!(m.oldest_commit_seq, 5);
    assert!(
        GraphDb::open_at(dir.path(), 7).is_err(),
        "asof cannot reach it either"
    );

    let into = tempfile::tempdir().unwrap();
    let target = into.path().join("restored");
    let e = restore_from_store(
        Arc::clone(&b.store),
        PREFIX,
        Some(RestoreTarget::CommitSeq(7)),
        &target,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, RestoreS3Error::Unreachable(_)), "{e}");
    assert!(!target.exists());
    assert!(dir_entries(into.path()).is_empty());
    // A commit in the live WAL beside the snapshot is reachable.
    let last = m.commit_seq - 1;
    restore_from_store(
        Arc::clone(&b.store),
        PREFIX,
        Some(RestoreTarget::CommitSeq(last)),
        &target,
    )
    .await
    .unwrap();
    assert_eq!(
        dump(&GraphDb::open(&target).unwrap()),
        dump(&GraphDb::open_at(dir.path(), last).unwrap())
    );
}

// ── Fix round 1 ──────────────────────────────────────────────────────────────

fn ms(s: &str) -> i64 {
    (rfc(s).unix_timestamp_nanos() / 1_000_000) as i64
}

fn rfc(s: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
}

/// The worker reads `commit_times.bin` after shipping the WAL, so a manifest
/// can carry the time of a commit it does not hold — here a commit that was
/// never fsynced, so never shipped, as when the source dies right after the
/// manifest. A restore of the newest point trims the times to its commits:
/// the restored store's next commit gets its own time, and a date resolves
/// to this store's commits.
#[tokio::test]
async fn a_restore_of_the_newest_point_trims_the_times_to_its_commits() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker(&b.store, &db, dir.path());
        db.write()
            .record_commits_at(Some(ms("2026-01-01T00:00:00Z")))
            .unwrap();
        insert(&db, "a", 2);
        w.run_once().await.unwrap();
        db.write()
            .record_commits_at(Some(ms("2026-02-01T00:00:00Z")))
            .unwrap();
        insert(&db, "c", 1);
        // Stamped and in the file, never fsynced: its time ships, it does not.
        db.write()
            .set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
        db.write()
            .record_commits_at(Some(ms("2026-03-01T00:00:00Z")))
            .unwrap();
        insert(&db, "d", 1);
        w.run_once().await.unwrap();
        let m = latest(&b.store).await;
        assert_eq!(m.commit_seq, 3, "{backend}");
        let times = m
            .files
            .iter()
            .find(|f| f.name == "commit_times.bin")
            .unwrap();
        let shipped = get(&b.store, &times.key).await.unwrap();
        assert!(
            mushroomdb::restore::commit_at_instant(&shipped, 0, i64::MAX, u64::MAX)
                .unwrap()
                .newest_recorded
                >= 3,
            "{backend}: the manifest's times run past its commits"
        );

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let out = restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
            .await
            .unwrap();
        assert_eq!(out.commits, 3, "{backend}");
        let mut r = GraphDb::open(&target).unwrap();
        assert_eq!(
            r.commit_time_ms(3),
            None,
            "{backend}: no time past its commits"
        );
        r.record_commits_at(Some(ms("2026-04-01T00:00:00Z")))
            .unwrap();
        r.insert_node("A", "z", vec![]).unwrap();
        assert_eq!(
            r.commit_time_ms(3),
            Some(ms("2026-04-01T00:00:00Z")),
            "{backend}: the next commit gets its own time"
        );
        assert_eq!(
            r.resolve_date("2026-03-15T00:00:00Z").unwrap(),
            2,
            "{backend}: a date between resolves to this store's commit, not the source's"
        );
    }
}

/// Segments shipped after the newest manifest hold commits whose times the
/// backup does not have. An instant at or after the newest recorded time
/// could fall after some of them, so it is refused, naming the limit; an
/// instant before it resolves as a date query would.
#[tokio::test]
async fn a_timestamp_past_the_recorded_times_is_refused_when_later_commits_have_none() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker_with(
            &b.store,
            &db,
            dir.path(),
            BackupConfig {
                manifest_interval_s: 3600,
                ..config()
            },
        );
        db.write()
            .record_commits_at(Some(ms("2026-01-01T00:00:00Z")))
            .unwrap();
        insert(&db, "a", 1);
        db.write()
            .record_commits_at(Some(ms("2026-02-01T00:00:00Z")))
            .unwrap();
        insert(&db, "b", 1);
        w.run_once().await.unwrap();
        db.write()
            .record_commits_at(Some(ms("2026-03-01T00:00:00Z")))
            .unwrap();
        insert(&db, "c", 2);
        w.run_once().await.unwrap();
        assert_eq!(latest(&b.store).await.commit_seq, 2, "{backend}: tail only");

        let into = tempfile::tempdir().unwrap();
        for at in ["2026-02-15T00:00:00Z", "2026-03-15T00:00:00Z"] {
            let target = into.path().join("refused");
            let e = restore_from_store(
                Arc::clone(&b.store),
                PREFIX,
                Some(RestoreTarget::Timestamp(rfc(at))),
                &target,
            )
            .await
            .unwrap_err();
            match &e {
                RestoreS3Error::CommitTimesEnd {
                    recorded_through,
                    recorded_through_ms,
                    commits,
                } => {
                    assert_eq!((*recorded_through, *commits), (1, 4), "{backend} {at}");
                    assert_eq!(*recorded_through_ms, ms("2026-02-01T00:00:00Z"));
                }
                other => panic!("{backend} {at}: expected CommitTimesEnd, got {other}"),
            }
            assert!(e.to_string().contains("--at <commit index>"), "{e}");
            assert!(e.to_string().contains("2026-02-01T00:00:00Z"), "{e}");
            assert!(!target.exists());
        }
        // Before the newest recorded time the answer is certain.
        let target = into.path().join("jan");
        let out = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::Timestamp(rfc("2026-01-15T00:00:00Z"))),
            &target,
        )
        .await
        .unwrap();
        assert_eq!(out.commits, 1, "{backend}");
        // And the tail's commits are reachable by number.
        let out = restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(3)),
            &into.path().join("by-number"),
        )
        .await
        .unwrap();
        assert_eq!(out.commits, 4, "{backend}");
    }
}

/// The install is one directory rename of a staged, fsynced store. A failure
/// at that last step — everything downloaded, verified, cut and staged —
/// leaves a missing target missing and an empty one empty: never a store
/// that opens, never a part of one, and no staging directory.
#[tokio::test]
async fn a_failed_install_leaves_the_target_absent_or_empty() {
    for (backend, b) in buckets() {
        let (_db, _dir) = two_generations(&b.store).await;
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("deep").join("restored");
        backup::restore::fail_next_install_into(&missing);
        let e = restore_from_store(Arc::clone(&b.store), PREFIX, None, &missing)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("injected"), "{backend}: {e}");
        assert!(!missing.exists(), "{backend}");
        assert!(dir_entries(parent.path()).is_empty(), "{backend}");

        let empty = tempfile::tempdir().unwrap();
        backup::restore::fail_next_install_into(empty.path());
        assert!(restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(75)),
            empty.path()
        )
        .await
        .is_err());
        assert!(dir_entries(empty.path()).is_empty(), "{backend}");
        assert!(!mushroomdb::restore::holds_a_store(empty.path()));

        // The same restores then succeed, whole, with nothing left beside.
        restore_from_store(Arc::clone(&b.store), PREFIX, None, &missing)
            .await
            .unwrap();
        assert!(mushroomdb::restore::holds_a_store(&missing), "{backend}");
        assert_eq!(
            dir_entries(missing.parent().unwrap()),
            ["restored"],
            "{backend}"
        );
        assert!(!missing.join("LOCK").exists(), "{backend}");
        restore_from_store(
            Arc::clone(&b.store),
            PREFIX,
            Some(RestoreTarget::CommitSeq(75)),
            empty.path(),
        )
        .await
        .unwrap();
        assert_eq!(
            GraphDb::open(empty.path())
                .unwrap()
                .wal_total_commits()
                .unwrap(),
            76,
            "{backend}"
        );
    }
}

// ── Fix round 2 ──────────────────────────────────────────────────────────────

/// A backup whose newest manifest is at commit 3, followed by two segments
/// of two commits each. Returns the bucket's two tail segment keys.
async fn manifest_then_two_tail_segments(b: &Bucket) -> (SharedDb, tempfile::TempDir, [String; 2]) {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut w = worker_with(
        &b.store,
        &db,
        dir.path(),
        BackupConfig {
            manifest_interval_s: 3600,
            ..config()
        },
    );
    insert(&db, "a", 3);
    w.run_once().await.unwrap();
    insert(&db, "b", 2);
    w.run_once().await.unwrap();
    insert(&db, "c", 2);
    w.run_once().await.unwrap();
    let m = latest(&b.store).await;
    assert_eq!(m.commit_seq, 3);
    let mut tail: Vec<String> = bucket_contents(&b.store)
        .await
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| {
            backup::manifest::parse_segment_key(PREFIX, k).is_some()
                && !m.segments.iter().any(|s| &s.key == k)
        })
        .collect();
    tail.sort();
    assert_eq!(tail.len(), 2, "{tail:?}");
    (db, dir, [tail[0].clone(), tail[1].clone()])
}

/// When the first segment after the newest manifest is missing or damaged,
/// none of the later commits can be restored. The restore of the newest
/// point still succeeds with what the manifest holds, and says how many
/// segments and bytes it did not restore and why; `--at` into the lost range
/// says so too. A restore of an older point, asked for, says nothing.
#[tokio::test]
async fn commits_after_the_manifest_that_cannot_be_restored_are_reported() {
    for (backend, b) in buckets() {
        let (_db, _dir, [first, _second]) = manifest_then_two_tail_segments(&b).await;
        let good = get(&b.store, &first).await.unwrap();
        for (case, damage) in [
            ("missing", None),
            ("damaged", Some(good[..good.len() - 1].to_vec())),
        ] {
            match &damage {
                None => b
                    .store
                    .delete(&ObjectPath::from(first.as_str()))
                    .await
                    .unwrap(),
                Some(bytes) => put(&b.store, &first, bytes.clone()).await,
            }
            let into = tempfile::tempdir().unwrap();
            let out =
                restore_from_store(Arc::clone(&b.store), PREFIX, None, &into.path().join("r"))
                    .await
                    .unwrap();
            assert_eq!(out.commits, 3, "{backend} {case}");
            let note = out
                .tail_note
                .unwrap_or_else(|| panic!("{backend} {case}: no warning"));
            assert!(note.contains("2 segment(s)"), "{backend} {case}: {note}");
            assert!(note.contains("not restored"), "{backend} {case}: {note}");
            assert!(note.contains(case), "{backend} {case}: {note}");

            let e = restore_from_store(
                Arc::clone(&b.store),
                PREFIX,
                Some(RestoreTarget::CommitSeq(5)),
                &into.path().join("at"),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    e,
                    RestoreS3Error::AfterLatest {
                        not_restorable: Some(_),
                        ..
                    }
                ),
                "{backend} {case}: {e}"
            );
            assert!(
                e.to_string().contains("not restored"),
                "{backend} {case}: {e}"
            );

            let older = restore_from_store(
                Arc::clone(&b.store),
                PREFIX,
                Some(RestoreTarget::CommitSeq(1)),
                &into.path().join("older"),
            )
            .await
            .unwrap();
            assert_eq!(older.tail_note, None, "{backend} {case}");
            put(&b.store, &first, good.clone()).await;
        }
        let into = tempfile::tempdir().unwrap();
        let whole = restore_from_store(Arc::clone(&b.store), PREFIX, None, &into.path().join("r"))
            .await
            .unwrap();
        assert_eq!((whole.commits, whole.tail_note), (7, None), "{backend}");
    }
}

/// A restore removes the staging directories and lock files killed restores
/// into the same target left beside it — and nothing else: not a running
/// restore's (its lock is held), not another target's, not a look-alike.
#[tokio::test]
async fn a_restore_sweeps_what_killed_restores_into_its_target_left() {
    let b = in_memory();
    let (_db, _dir) = two_generations(&b.store).await;
    let parent = tempfile::tempdir().unwrap();
    let p = parent.path();
    let stale = p.join(".restored.restore-4242-0a1b2c3d");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("snapshot.bin"), b"half a store").unwrap();
    std::fs::write(p.join(".restored.restore-4242-0a1b2c3d.lock"), b"").unwrap();
    let orphan_lock = p.join(".restored.restore-17-ffffffff.lock");
    std::fs::write(&orphan_lock, b"").unwrap();
    // A restore still running: its lock file is held.
    let running = p.join(".restored.restore-99-12345678");
    std::fs::create_dir(&running).unwrap();
    let held = std::fs::File::create(p.join(".restored.restore-99-12345678.lock")).unwrap();
    held.lock().unwrap();
    // Not this tool's, or not this target's.
    for keep in [
        ".restored.restore-x",
        ".restored.restore-1-ABCDEF01",
        ".restored.restore-1-abc",
        ".other.restore-1-abcdef01",
        "restored.restore-1-abcdef01",
    ] {
        std::fs::create_dir(p.join(keep)).unwrap();
    }

    let out = restore_from_store(Arc::clone(&b.store), PREFIX, None, &p.join("restored"))
        .await
        .unwrap();
    let mut swept = out.swept.clone();
    swept.sort();
    let mut want = vec![
        stale.clone(),
        p.join(".restored.restore-4242-0a1b2c3d.lock"),
        orphan_lock,
    ];
    want.sort();
    assert_eq!(swept, want);
    assert_eq!(
        dir_entries(p),
        [
            ".other.restore-1-abcdef01",
            ".restored.restore-1-ABCDEF01",
            ".restored.restore-1-abc",
            ".restored.restore-99-12345678",
            ".restored.restore-99-12345678.lock",
            ".restored.restore-x",
            "restored",
            "restored.restore-1-abcdef01",
        ]
    );
    drop(held);
}

// ── TY-12 × HO-09: a V11 store restores as V11 ───────────────────────────────

/// The snapshot versions a 0.9.x reader accepts (V5–V10); it refuses every
/// other one with `snapshot: unsupported version {N}` (see
/// `crates/core-api/tests/snapshot_v11.rs`).
const V0_9_ACCEPTS: [u16; 6] = [5, 6, 7, 8, 9, 10];

fn event(db: &SharedDb, key: &str, on: i64) {
    db.write()
        .insert_node(
            "Event",
            key,
            vec![("on".into(), mushroomdb::Value::Int(on))],
        )
        .unwrap();
}

fn events(db: &SharedDb, from: i64, n: i64) {
    for i in from..from + n {
        event(db, &format!("e{i}"), i);
    }
}

fn commits_of(db: &SharedDb) -> u64 {
    db.read().wal_total_commits().unwrap()
}

/// Declare a constraint and a range index on `Event.on`: the store's first
/// V11-only records, so the store moves to V11 at the first. Returns the
/// commit index of the constraint, the first commit at which it is V11.
fn make_v11(db: &SharedDb) -> u64 {
    let first = commits_of(db);
    {
        let mut g = db.write();
        assert!(!g.is_v11(), "not V11 before its first V11-only record");
        g.create_constraint("uniq_on", "Event", "on").unwrap();
        g.enable_range_index("by_on", "Event", "on").unwrap();
        assert!(g.is_v11());
    }
    assert_eq!(commits_of(db), first + 2, "one commit per declaration");
    first
}

/// The store's whole retained history as WAL bytes: its archives in order,
/// then its live WAL.
fn history_bytes(dir: &Path) -> Vec<u8> {
    let mut archives: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let name = p.file_name()?.to_str()?.to_string();
            let n = name
                .strip_prefix("wal.")?
                .strip_suffix(".archive")?
                .parse()
                .ok()?;
            Some((n, p))
        })
        .collect();
    archives.sort();
    let mut bytes = Vec::new();
    for (_, p) in archives {
        bytes.extend(std::fs::read(p).unwrap());
    }
    bytes.extend(std::fs::read(dir.join("wal.bin")).unwrap_or_default());
    bytes
}

/// A store restored at `commit` from a V11 source (source directory `src`,
/// V11 from commit `v11_from`) is the source at that commit, byte for byte,
/// and is V11 exactly when the source was V11 there: once V11, every later
/// commit is V11 (C9). `snapshot_dropped`: the cut is one of the two that
/// remove the snapshot (the commit is in an archive, or in a WAL the snapshot
/// kept).
fn assert_cut_matches_source(
    what: &str,
    src: &Path,
    target: &Path,
    commit: u64,
    v11_from: u64,
    snapshot_dropped: bool,
) {
    let want_v11 = commit >= v11_from;
    assert_eq!(
        !target.join("snapshot.bin").exists(),
        snapshot_dropped,
        "{what}: {:?}",
        dir_entries(target)
    );
    // Byte-exact: the WAL is a prefix of the history it was cut from (the
    // source's live WAL beside a kept snapshot; its archives and WAL rebuilt
    // from nothing otherwise) holding exactly `commit + 1` commits.
    let wal = std::fs::read(target.join("wal.bin")).unwrap();
    let from = if snapshot_dropped {
        history_bytes(src)
    } else {
        assert_eq!(
            std::fs::read(target.join("snapshot.bin")).unwrap(),
            std::fs::read(src.join("snapshot.bin")).unwrap(),
            "{what}: the kept snapshot is the source's"
        );
        std::fs::read(src.join("wal.bin")).unwrap()
    };
    assert!(
        from.starts_with(&wal),
        "{what}: the WAL is a prefix of the source's"
    );
    let restored = GraphDb::open(target).unwrap();
    assert_eq!(restored.wal_total_commits().unwrap(), commit + 1, "{what}");
    assert_eq!(
        dump(&restored),
        dump(&GraphDb::open_at(src, commit).unwrap()),
        "{what}: the graph at commit {commit}"
    );
    assert_eq!(
        restored.is_v11(),
        want_v11,
        "{what}: V11 iff the source was"
    );
    assert_eq!(
        restored.constraints().len(),
        usize::from(want_v11),
        "{what}"
    );
    assert_eq!(
        restored.range_indexes().len(),
        usize::from(commit > v11_from),
        "{what}"
    );
    drop(restored);

    // It stays what it is: a write and a snapshot stamp V11 iff it was V11.
    let mut db = GraphDb::open(target).unwrap();
    if want_v11 {
        assert!(
            db.insert_node(
                "Event",
                "dup",
                vec![("on".into(), mushroomdb::Value::Int(0))]
            )
            .is_err(),
            "{what}: the constraint is enforced"
        );
    }
    db.insert_node("Event", "after-restore", vec![]).unwrap();
    db.snapshot().unwrap();
    let version = mushroomdb::snapshot_version_at(target).unwrap().unwrap();
    assert_eq!(version == 11, want_v11, "{what}: stamped V{version}");
}

/// A store made V11 by a constraint and a range index backs up through the
/// worker and restores through `restore_from_store` as V11: stamped V11, its
/// constraint and range index present and the constraint enforced, the V11
/// genesis marker in place of the one 0.9.x reads, and a version 0.9.x
/// refuses.
#[tokio::test]
async fn a_v11_store_restores_as_v11() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker(&b.store, &db, dir.path());
        events(&db, 0, 5);
        db.write().snapshot_with(ARCHIVING).unwrap();
        make_v11(&db);
        events(&db, 5, 5);
        db.write().snapshot_with(ARCHIVING).unwrap();
        events(&db, 10, 5);
        w.run_once().await.unwrap();
        w.flush().await;
        assert_eq!(
            mushroomdb::snapshot_version_at(dir.path()).unwrap(),
            Some(11)
        );

        let into = tempfile::tempdir().unwrap();
        let target = into.path().join("restored");
        let out = restore_from_store(Arc::clone(&b.store), PREFIX, None, &target)
            .await
            .unwrap_or_else(|e| panic!("{backend}: {e}"));
        assert_eq!(out.commits, commits_of(&db), "{backend}");
        let version = mushroomdb::snapshot_version_at(&target).unwrap();
        assert_eq!(version, Some(11), "{backend}");
        assert!(
            !V0_9_ACCEPTS.contains(&version.unwrap()),
            "{backend}: a 0.9.x reader refuses it by version"
        );
        assert!(target.join("wal.genesis.v11").exists(), "{backend}");
        assert!(!target.join("wal.genesis").exists(), "{backend}");
        assert_eq!(
            std::fs::read(target.join("snapshot.bin")).unwrap(),
            std::fs::read(dir.path().join("snapshot.bin")).unwrap(),
            "{backend}: the snapshot byte for byte"
        );

        let mut back = GraphDb::open(&target).unwrap();
        assert!(back.is_v11(), "{backend}");
        assert_eq!(dump(&back), dump(&db.read()), "{backend}");
        assert_eq!(back.constraints().len(), 1, "{backend}");
        assert_eq!(
            back.range_indexes(),
            vec![("by_on".into(), "Event".into(), "on".into())],
            "{backend}"
        );
        assert!(
            back.insert_node(
                "Event",
                "dup",
                vec![("on".into(), mushroomdb::Value::Int(3))]
            )
            .is_err(),
            "{backend}: the constraint is enforced"
        );
        back.insert_node(
            "Event",
            "new",
            vec![("on".into(), mushroomdb::Value::Int(99))],
        )
        .unwrap();
        back.snapshot().unwrap();
        assert_eq!(
            mushroomdb::snapshot_version_at(&target).unwrap(),
            Some(11),
            "{backend}: it stays V11"
        );
    }
}

/// `cut_at_commit` through a restore, on a V11 store whose snapshot kept its
/// WAL (the upgrade's own snapshot; no archives): the cut drops the snapshot
/// and replays the WAL prefix. Before the first V11 record the restored store
/// is not V11; at it and after, it is.
#[tokio::test]
async fn a_cut_in_the_wal_a_v11_snapshot_kept_restores_v11_iff_the_source_was() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker(&b.store, &db, dir.path());
        events(&db, 0, 10);
        let v11_from = make_v11(&db);
        events(&db, 10, 10);
        w.run_once().await.unwrap();
        w.flush().await;
        assert_eq!(
            mushroomdb::snapshot_version_at(dir.path()).unwrap(),
            Some(11),
            "the upgrade wrote a V11 snapshot"
        );
        let last = commits_of(&db) - 1;
        for commit in [0, v11_from - 1, v11_from, v11_from + 1, v11_from + 5, last] {
            let what = format!("{backend}, commit {commit}");
            let into = tempfile::tempdir().unwrap();
            let target = into.path().join("restored");
            let out = restore_from_store(
                Arc::clone(&b.store),
                PREFIX,
                Some(RestoreTarget::CommitSeq(commit)),
                &target,
            )
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(out.commits, commit + 1, "{what}");
            assert_cut_matches_source(&what, dir.path(), &target, commit, v11_from, true);
        }
    }
}

/// `cut_at_commit` through a restore, on a V11 store with archives on both
/// sides of the move to V11: a commit in an archive drops the V11 snapshot and
/// rebuilds from nothing; a commit in the live WAL keeps it. The restored
/// store is the source at that commit, and V11 iff the source was.
#[tokio::test]
async fn a_cut_in_an_archive_of_a_v11_store_restores_v11_iff_the_source_was() {
    for (backend, b) in buckets() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDb::open(dir.path()).unwrap();
        let mut w = worker(&b.store, &db, dir.path());
        events(&db, 0, 10);
        db.write().snapshot_with(ARCHIVING).unwrap();
        events(&db, 10, 5);
        let v11_from = make_v11(&db);
        events(&db, 15, 5);
        db.write().snapshot_with(ARCHIVING).unwrap();
        let live_from = commits_of(&db);
        events(&db, 20, 5);
        w.run_once().await.unwrap();
        w.flush().await;
        assert_eq!(
            mushroomdb::snapshot_version_at(dir.path()).unwrap(),
            Some(11)
        );
        let last = commits_of(&db) - 1;
        for commit in [
            3,            // the first archive, long before V11
            v11_from - 2, // the second archive, before V11
            v11_from,     // the second archive, the first V11 commit
            v11_from + 4, // the second archive, after
            live_from,    // the live WAL beside the V11 snapshot
            last,
        ] {
            let what = format!("{backend}, commit {commit}");
            let into = tempfile::tempdir().unwrap();
            let target = into.path().join("restored");
            let out = restore_from_store(
                Arc::clone(&b.store),
                PREFIX,
                Some(RestoreTarget::CommitSeq(commit)),
                &target,
            )
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(out.commits, commit + 1, "{what}");
            assert_cut_matches_source(
                &what,
                dir.path(),
                &target,
                commit,
                v11_from,
                commit < live_from,
            );
        }
    }
}
