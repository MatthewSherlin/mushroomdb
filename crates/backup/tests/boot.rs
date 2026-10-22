//! Restore on boot (HO-10): an empty data directory restores the newest
//! restore point of a prefix, in place, under the bucket lease, and its
//! backup then continues under the same prefix as a new lineage that never
//! overwrites or deletes the parent's chain (defect-ledger row 138).

use backup::lease::{lease_key, LeaseRecord};
use backup::manifest::latest_key;
use backup::restore::{
    inject_fault, install_interrupted, restore_from_store, Fault, RestoreS3Error, INSTALL_MARKER,
    STALE_STAGING_AFTER,
};
use backup::{
    restore_on_boot, BackupConfig, BackupError, BackupTrigger, BackupWorker, BootError,
    BootRestore, Manifest,
};
use mushroomdb::{GraphDb, SharedDb, SnapshotOptions};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PREFIX: &str = "pfx";

const ARCHIVING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: true,
};

const TRUNCATING: SnapshotOptions = SnapshotOptions {
    keep_wal: false,
    archive_wal: false,
};

fn mem() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
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

fn insert(db: &SharedDb, prefix: &str, n: usize) {
    for i in 0..n {
        db.write()
            .insert_node("A", &format!("{prefix}{i}"), vec![])
            .unwrap();
    }
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

async fn latest_target(store: &Arc<dyn ObjectStore>) -> String {
    String::from_utf8(get(store, &latest_key(PREFIX)).await.unwrap()).unwrap()
}

async fn latest(store: &Arc<dyn ObjectStore>) -> Manifest {
    let key = latest_target(store).await;
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

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn lineage_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("backup.lineage"))
        .unwrap()
        .trim()
        .to_string()
}

type Dump = (Vec<mushroomdb::NodeInfo>, Vec<mushroomdb::ExportEdge>);

fn dump_of(dir: &Path) -> Dump {
    let db = GraphDb::open(dir).unwrap();
    (db.all_nodes_for_export(), db.all_edges_for_export())
}

/// A source store backed up to `PREFIX` across two generations, then
/// closed. Returns its directory.
async fn source(store: &Arc<dyn ObjectStore>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    let mut w = worker_with(store, &db, dir.path(), config());
    insert(&db, "a", 20);
    db.write().insert_edge("L", "a0", "a1").unwrap();
    w.run_once().await.unwrap();
    db.write().snapshot_with(ARCHIVING).unwrap();
    insert(&db, "b", 10);
    w.run_once().await.unwrap();
    drop(w);
    drop(db);
    dir
}

/// A data directory that does not exist yet, inside a temporary directory
/// (a mount point's parent is not ours to write).
fn data_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("data");
    (root, dir)
}

fn restored(r: BootRestore) -> (backup::RestoreS3Outcome, backup::BootLease) {
    match r {
        BootRestore::Restored { outcome, lease, .. } => (*outcome, lease),
        other => panic!("expected a restore, got {other:?}"),
    }
}

// ── Lineage: row 138 ─────────────────────────────────────────────────────────

/// A restore on boot mints a new lineage and records the parent's lineage
/// and restore point. Its backup continues under the same prefix: LATEST
/// moves to the new lineage's manifests, retention runs (keep one) across a
/// new generation, and the parent's chain is byte-for-byte what it was —
/// its newest restore point still restores to the source, bit for bit.
#[tokio::test]
async fn a_boot_restore_continues_the_prefix_as_a_new_lineage_without_touching_the_parent() {
    let store = mem();
    let src = source(&store).await;
    let parent = latest(&store).await;
    let parent_key = latest_target(&store).await;
    let parent_objects: Vec<(String, Vec<u8>)> = bucket_contents(&store)
        .await
        .into_iter()
        .filter(|(k, _)| !k.ends_with("/LATEST") && !k.ends_with("/LEASE"))
        .collect();

    let (_root, dir) = data_dir();
    let (out, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), &dir)
            .await
            .unwrap(),
    );
    assert_eq!(out.source_lineage, parent.lineage);
    assert_ne!(out.lineage, parent.lineage, "a new lineage");
    assert_eq!(lineage_of(&dir), out.lineage);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("backup.parent")).unwrap()).unwrap();
    assert_eq!(record["lineage"], parent.lineage);
    assert_eq!(record["manifest"], parent_key);
    assert_eq!(record["commit_seq"], out.commits);
    assert_eq!(dump_of(&dir), dump_of(src.path()));
    assert!(!dir.join(INSTALL_MARKER).exists());

    // Serve: hold the boot's lease, write, ship, snapshot, ship again, with
    // retention keeping one manifest.
    let db = SharedDb::open(&dir).unwrap();
    let mut w = worker_with(
        &store,
        &db,
        &dir,
        BackupConfig {
            retain_n: 1,
            ..config()
        },
    );
    w.adopt_lease(lease.into_lease().await);
    w.run_once().await.unwrap();
    assert_eq!(
        latest_target(&store).await,
        parent_key,
        "nothing new: the parent's restore point holds this state, and is not overwritten"
    );
    insert(&db, "child", 5);
    w.run_once().await.unwrap();
    let first = latest(&store).await;
    assert_eq!(
        first.lineage, out.lineage,
        "LATEST moved to the new lineage"
    );
    assert!(first.wal_generation > parent.wal_generation);
    // A truncating snapshot: the new generation's chain files (wal.floor,
    // wal.genesis) differ from the parent's, so retention drops the only
    // child manifest naming the side files it shares with the parent.
    db.write().snapshot_with(TRUNCATING).unwrap();
    insert(&db, "more", 5);
    w.run_once().await.unwrap();
    insert(&db, "again", 3);
    w.run_once().await.unwrap();
    let newest = latest(&store).await;
    assert_eq!(newest.lineage, out.lineage);
    assert_ne!(newest.wal_generation, first.wal_generation);
    drop(w);
    drop(db);

    // The parent's chain: every object, byte for byte.
    let now: std::collections::HashMap<String, Vec<u8>> =
        bucket_contents(&store).await.into_iter().collect();
    for (k, bytes) in &parent_objects {
        assert_eq!(now.get(k), Some(bytes), "{k} of the parent changed or went");
    }
    // ... and its newest restore point restores to the source.
    let replica = mem();
    for (k, bytes) in &parent_objects {
        put(&replica, k, bytes.clone()).await;
    }
    put(
        &replica,
        &latest_key(PREFIX),
        parent_key.clone().into_bytes(),
    )
    .await;
    let again = tempfile::tempdir().unwrap();
    let into = again.path().join("parent");
    let o = restore_from_store(replica, PREFIX, None, &into)
        .await
        .unwrap();
    assert_eq!(o.source_lineage, parent.lineage);
    assert_eq!(dump_of(&into), dump_of(src.path()));

    // And the child's own newest restore point restores the child.
    let child = again.path().join("child");
    restore_from_store(Arc::clone(&store), PREFIX, None, &child)
        .await
        .unwrap();
    assert_eq!(dump_of(&child), dump_of(&dir));
}

/// A store restored on boot whose prefix moved on since (another instance
/// wrote a newer restore point of the parent lineage) is refused: nothing is
/// uploaded or deleted.
#[tokio::test]
async fn a_prefix_that_moved_on_since_the_boot_restore_is_refused() {
    let store = mem();
    let src = tempfile::tempdir().unwrap();
    let db = SharedDb::open(src.path()).unwrap();
    let mut w = worker_with(&store, &db, src.path(), config());
    insert(&db, "a", 5);
    w.run_once().await.unwrap();

    let (_root, dir) = data_dir();
    let (_, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), &dir)
            .await
            .unwrap(),
    );
    lease.release().await;
    // The source keeps writing to the prefix after the restore.
    insert(&db, "late", 3);
    w.run_once().await.unwrap();
    drop(w);

    let child = SharedDb::open(&dir).unwrap();
    insert(&child, "c", 10);
    let before = bucket_contents(&store).await;
    let mut cw = worker_with(&store, &child, &dir, config());
    match cw.run_once().await.unwrap_err() {
        BackupError::Lineage(m) => assert!(m.contains("moved on"), "{m}"),
        other => panic!("expected a lineage refusal, got {other}"),
    }
    assert_eq!(bucket_contents(&store).await, before);
}

/// The restore on boot is the only restore that continues its source
/// prefix: a store restored with `restore --from` has no parent record and
/// is still refused there (HO-09's rule), and so is one whose parent record
/// names another lineage.
#[tokio::test]
async fn only_a_boot_restore_record_continues_the_prefix() {
    let store = mem();
    let _src = source(&store).await;
    let tmp = tempfile::tempdir().unwrap();
    let cli = tmp.path().join("cli");
    restore_from_store(Arc::clone(&store), PREFIX, None, &cli)
        .await
        .unwrap();
    assert!(!cli.join("backup.parent").exists());
    let forged = tmp.path().join("forged");
    restore_from_store(Arc::clone(&store), PREFIX, None, &forged)
        .await
        .unwrap();
    std::fs::write(
        forged.join("backup.parent"),
        format!(
            r#"{{"lineage":"someone-else","manifest":"{}","commit_seq":1}}"#,
            latest_target(&store).await
        ),
    )
    .unwrap();
    for dir in [cli, forged] {
        let db = SharedDb::open(&dir).unwrap();
        insert(&db, "grown", 40);
        let before = bucket_contents(&store).await;
        let mut w = worker_with(&store, &db, &dir, config());
        match w.run_once().await.unwrap_err() {
            BackupError::Lineage(m) => assert!(m.contains("another store"), "{m}"),
            other => panic!("expected a lineage refusal, got {other}"),
        }
        assert_eq!(bucket_contents(&store).await, before, "{}", dir.display());
    }
}

// ── The lease ────────────────────────────────────────────────────────────────

async fn read_lease(store: &Arc<dyn ObjectStore>) -> Option<LeaseRecord> {
    get(store, &lease_key(PREFIX))
        .await
        .map(|b| serde_json::from_slice(&b).unwrap())
}

/// The lease is taken before the restore and held into serve: the worker
/// that adopts it renews it and never takes it again (the record's
/// `acquired_at` and instance id stay the boot's), and its shutdown deletes
/// it.
#[tokio::test]
async fn the_boot_lease_is_held_into_serve_and_never_taken_again() {
    let store = mem();
    let _src = source(&store).await;
    let (_root, dir) = data_dir();
    let c = BackupConfig {
        lease_ttl_s: 3,
        wal_interval_ms: 50,
        ..config()
    };
    let (_, lease) = restored(restore_on_boot(&c, Arc::clone(&store), &dir).await.unwrap());
    let taken = read_lease(&store).await.expect("the lease is held");
    assert_eq!(taken.instance_id, lease.instance_id());
    // Held across a whole TTL before the worker exists (a slow open).
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let renewed = read_lease(&store).await.unwrap();
    assert_eq!(renewed.instance_id, taken.instance_id);
    assert_ne!(renewed.renewed_at, taken.renewed_at, "renewed meanwhile");

    let db = SharedDb::open(&dir).unwrap();
    let mut w = worker_with(&store, &db, &dir, c);
    w.adopt_lease(lease.into_lease().await);
    assert_eq!(w.instance_id(), taken.instance_id);
    let (tx, rx) = tokio::sync::watch::channel(BackupTrigger::Idle);
    let handle = w.start(rx);
    insert(&db, "served", 3);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while latest(&store).await.lineage != lineage_of(&dir) {
        assert!(tokio::time::Instant::now() < deadline, "nothing shipped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let held = read_lease(&store).await.unwrap();
    assert_eq!(held.instance_id, taken.instance_id, "the same holder");
    assert_eq!(held.acquired_at, taken.acquired_at, "never taken again");
    assert!(handle.healthy.load(std::sync::atomic::Ordering::Acquire));
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    assert!(read_lease(&store).await.is_none(), "released at shutdown");
}

/// Another instance's fresh lease: nothing is restored and nothing is
/// written to the data directory; the error names the holder.
#[tokio::test]
async fn a_live_foreign_lease_refuses_the_restore_and_writes_nothing() {
    let store = mem();
    let _src = source(&store).await;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let record = LeaseRecord {
        instance_id: "the-other-one".into(),
        acquired_at: now.clone(),
        renewed_at: now,
        ttl_s: 600,
    };
    put(
        &store,
        &lease_key(PREFIX),
        serde_json::to_vec(&record).unwrap(),
    )
    .await;
    let (root, dir) = data_dir();
    let e = restore_on_boot(&config(), Arc::clone(&store), &dir)
        .await
        .unwrap_err();
    assert!(matches!(e, BootError::ForeignLease { .. }), "{e}");
    assert!(e.to_string().contains("the-other-one"), "{e}");
    assert!(!dir.exists(), "nothing was written");
    assert!(dir_entries(root.path()).is_empty());
    assert_eq!(read_lease(&store).await.unwrap(), record);
}

// ── What the data directory holds ───────────────────────────────────────────

/// A data directory that holds a store is never restored over; no lease is
/// taken.
#[tokio::test]
async fn a_data_dir_holding_a_store_is_skipped() {
    let store = mem();
    let _src = source(&store).await;
    let dir = tempfile::tempdir().unwrap();
    {
        let mut db = GraphDb::open(dir.path()).unwrap();
        db.insert_node("Local", "mine", vec![]).unwrap();
    }
    let before = dir_entries(dir.path());
    let r = restore_on_boot(&config(), Arc::clone(&store), dir.path())
        .await
        .unwrap();
    assert!(matches!(r, BootRestore::Skipped), "{r:?}");
    assert_eq!(dir_entries(dir.path()), before);
    assert!(GraphDb::open(dir.path()).unwrap().has_node("mine"));
    assert!(read_lease(&store).await.is_none());
}

/// A prefix with no backup at all starts an empty store: the first deploy
/// of a template. The lease is held for the worker.
#[tokio::test]
async fn an_empty_prefix_is_no_backup_and_keeps_the_lease() {
    let store = mem();
    let (_root, dir) = data_dir();
    match restore_on_boot(&config(), Arc::clone(&store), &dir)
        .await
        .unwrap()
    {
        BootRestore::NoBackup { lease } => {
            assert_eq!(
                read_lease(&store).await.unwrap().instance_id,
                lease.instance_id()
            );
            lease.release().await;
        }
        other => panic!("expected no backup, got {other:?}"),
    }
    assert!(!dir.exists() || dir_entries(&dir).is_empty());
}

/// A restore point that does not verify fails the boot, leaves the data
/// directory without a store, and gives the lease back.
#[tokio::test]
async fn a_failed_boot_restore_leaves_no_store_and_releases_the_lease() {
    let store = mem();
    let _src = source(&store).await;
    let m = latest(&store).await;
    let seg = &m.segments[0].key;
    let mut bytes = get(&store, seg).await.unwrap();
    bytes[0] ^= 0xff;
    put(&store, seg, bytes).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("lost+found")).unwrap();
    let e = restore_on_boot(&config(), Arc::clone(&store), dir.path())
        .await
        .unwrap_err();
    assert!(
        matches!(e, BootError::Restore(RestoreS3Error::Verify(_))),
        "{e}"
    );
    assert_eq!(dir_entries(dir.path()), ["lost+found"]);
    assert!(!mushroomdb::restore::holds_a_store(dir.path()));
    assert!(read_lease(&store).await.is_none(), "released");
}

/// A data directory that holds no store but holds other files is not
/// restored into: the files are named, and left alone.
#[tokio::test]
async fn a_data_dir_with_stray_files_is_refused_by_name() {
    let store = mem();
    let _src = source(&store).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"mine").unwrap();
    let e = restore_on_boot(&config(), Arc::clone(&store), dir.path())
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            BootError::Restore(RestoreS3Error::DataDirNotEmpty { .. })
        ),
        "{e}"
    );
    assert!(e.to_string().contains("notes.txt"), "{e}");
    assert_eq!(dir_entries(dir.path()), ["notes.txt"]);
}

/// Set a file's (or directory's) modification time `age` in the past.
fn age(path: &Path, by: Duration) {
    let t = std::time::SystemTime::now() - by;
    std::fs::File::open(path).unwrap().set_modified(t).unwrap();
}

/// A data directory holding only `lost+found`, a `LOCK` file, an empty
/// `wal.bin` and what killed restores into it left counts as empty: the
/// leftovers are swept, and the store is restored beside `lost+found`.
#[tokio::test]
async fn leftovers_of_dead_restores_and_lost_found_count_as_empty() {
    let store = mem();
    let src = source(&store).await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let name = d.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir(d.join("lost+found")).unwrap();
    std::fs::write(d.join("LOCK"), b"").unwrap();
    std::fs::write(d.join("wal.bin"), b"").unwrap();
    let stale = d.join(format!(".{name}.restore-4242-0a1b2c3d"));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("snapshot.bin"), b"half a store").unwrap();
    let stale_lock = d.join(format!(".{name}.restore-4242-0a1b2c3d.lock"));
    std::fs::write(&stale_lock, b"").unwrap();
    age(&stale_lock, STALE_STAGING_AFTER * 2);
    age(&stale, STALE_STAGING_AFTER * 2);

    let (out, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), d)
            .await
            .unwrap(),
    );
    lease.release().await;
    let mut swept = out.swept.clone();
    swept.sort();
    assert_eq!(swept, [stale, stale_lock]);
    assert!(dir_entries(d).contains(&"lost+found".to_string()));
    assert!(!dir_entries(d).iter().any(|e| e.contains(".restore-")));
    assert_eq!(dump_of(d), dump_of(src.path()));
}

/// A crash while the files were being moved in leaves the install marker:
/// a plain open would see a part of a store, so the marker is visible
/// ([`install_interrupted`]), and the next boot removes exactly the files it
/// names — not a file it does not name — then restores the whole store.
#[tokio::test]
async fn an_interrupted_install_is_undone_at_the_next_boot() {
    let store = mem();
    let src = source(&store).await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // What a crash after moving two files in leaves.
    std::fs::write(d.join("snapshot.bin"), b"part of a store").unwrap();
    std::fs::write(d.join("wal.floor"), b"12345678").unwrap();
    std::fs::write(
        d.join(INSTALL_MARKER),
        "wal.floor\nsnapshot.bin\nwal.bin\n../escape",
    )
    .unwrap();
    std::fs::create_dir(d.join("lost+found")).unwrap();
    assert!(install_interrupted(d));
    assert!(
        mushroomdb::restore::holds_a_store(d),
        "it looks like a store"
    );

    match restore_on_boot(&config(), Arc::clone(&store), d)
        .await
        .unwrap()
    {
        BootRestore::Restored {
            mut undone, lease, ..
        } => {
            undone.sort();
            assert_eq!(undone, ["snapshot.bin", "wal.floor"]);
            lease.release().await;
        }
        other => panic!("expected a restore, got {other:?}"),
    }
    assert!(!install_interrupted(d));
    assert_eq!(dump_of(d), dump_of(src.path()));
}

/// A failure while the files are moved in removes what was moved and the
/// marker: the data directory is as it was, and the next boot restores.
#[tokio::test]
async fn a_failure_halfway_through_the_install_moves_nothing_in() {
    let store = mem();
    let src = source(&store).await;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir(d.join("lost+found")).unwrap();
    inject_fault(d, Fault::AfterMoving(2));
    let e = restore_on_boot(&config(), Arc::clone(&store), d)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("injected"), "{e}");
    assert_eq!(dir_entries(d), ["lost+found"]);
    assert!(read_lease(&store).await.is_none());

    let (_, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), d)
            .await
            .unwrap(),
    );
    lease.release().await;
    assert_eq!(dump_of(d), dump_of(src.path()));
}

// ── Fix round 1 ──────────────────────────────────────────────────────────────

/// M1: the install has succeeded only once the marker's removal is durable.
/// A failed fsync there fails the boot (nothing is served) and gives the
/// lease back; the store is in place without the marker, so the next boot
/// serves it.
#[tokio::test]
async fn a_failed_sync_after_the_marker_removal_fails_the_boot() {
    let store = mem();
    let src = source(&store).await;
    let (_root, dir) = data_dir();
    inject_fault(&dir, Fault::MarkerRemovalSync);
    let e = restore_on_boot(&config(), Arc::clone(&store), &dir)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("install marker"), "{e}");
    assert!(read_lease(&store).await.is_none(), "released");
    assert!(!install_interrupted(&dir));
    let r = restore_on_boot(&config(), Arc::clone(&store), &dir)
        .await
        .unwrap();
    assert!(matches!(r, BootRestore::Skipped), "{r:?}");
    assert_eq!(dump_of(&dir), dump_of(src.path()));
}

/// M1: a marker that came back (its removal lost to a power loss) over a
/// store that has served writes since is not undone: the boot refuses,
/// naming the marker, and deletes nothing. Over a store that has not
/// changed, the next boot undoes and restores again.
#[tokio::test]
async fn a_marker_back_over_a_store_that_served_is_refused_not_undone() {
    let store = mem();
    let src = source(&store).await;

    // Unchanged since the install: undone and restored again.
    let (_r1, quiet) = data_dir();
    inject_fault(&quiet, Fault::KeepMarker);
    let (_, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), &quiet)
            .await
            .unwrap(),
    );
    lease.release().await;
    assert!(install_interrupted(&quiet));
    let (_, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), &quiet)
            .await
            .unwrap(),
    );
    lease.release().await;
    assert!(!install_interrupted(&quiet));
    assert_eq!(dump_of(&quiet), dump_of(src.path()));

    // Served writes since: refused, nothing removed.
    let (_r2, served) = data_dir();
    inject_fault(&served, Fault::KeepMarker);
    let (_, lease) = restored(
        restore_on_boot(&config(), Arc::clone(&store), &served)
            .await
            .unwrap(),
    );
    lease.release().await;
    std::fs::rename(served.join(INSTALL_MARKER), served.join("marker.keep")).unwrap();
    {
        let db = SharedDb::open(&served).unwrap();
        insert(&db, "acked", 3);
    }
    std::fs::rename(served.join("marker.keep"), served.join(INSTALL_MARKER)).unwrap();
    let before = dir_entries(&served);
    let e = restore_on_boot(&config(), Arc::clone(&store), &served)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("may have served"), "{e}");
    assert!(e.to_string().contains(INSTALL_MARKER), "{e}");
    assert_eq!(dir_entries(&served), before, "nothing removed");
    assert!(read_lease(&store).await.is_none(), "released");
    std::fs::remove_file(served.join(INSTALL_MARKER)).unwrap();
    assert!(GraphDb::open(&served).unwrap().has_node("acked0"));
}

/// M4: under another instance's live lease, an interrupted install is left
/// exactly as it is: the undo runs only under this boot's own lease.
#[tokio::test]
async fn a_live_foreign_lease_leaves_an_interrupted_install_alone() {
    let store = mem();
    let _src = source(&store).await;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let record = LeaseRecord {
        instance_id: "the-other-one".into(),
        acquired_at: now.clone(),
        renewed_at: now,
        ttl_s: 600,
    };
    put(
        &store,
        &lease_key(PREFIX),
        serde_json::to_vec(&record).unwrap(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("snapshot.bin"), b"part of a store").unwrap();
    std::fs::write(dir.path().join(INSTALL_MARKER), "snapshot.bin").unwrap();
    let before = dir_entries(dir.path());
    let e = restore_on_boot(&config(), Arc::clone(&store), dir.path())
        .await
        .unwrap_err();
    assert!(matches!(e, BootError::ForeignLease { .. }), "{e}");
    assert_eq!(dir_entries(dir.path()), before);
}

/// M3: a boot that skips the restore (the directory holds a store) still
/// sweeps what killed restores into it left, once they are stale.
#[tokio::test]
async fn a_skipped_boot_sweeps_stale_leftovers() {
    let store = mem();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    {
        let mut db = GraphDb::open(d).unwrap();
        db.insert_node("Local", "mine", vec![]).unwrap();
    }
    let name = d.file_name().unwrap().to_string_lossy().into_owned();
    let stale = d.join(format!(".{name}.restore-4242-0a1b2c3d"));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("snapshot.bin"), b"a whole old copy").unwrap();
    age(&stale, STALE_STAGING_AFTER * 2);
    let r = restore_on_boot(&config(), Arc::clone(&store), d)
        .await
        .unwrap();
    assert!(matches!(r, BootRestore::Skipped), "{r:?}");
    assert!(!stale.exists());
    assert!(GraphDb::open(d).unwrap().has_node("mine"));
}
