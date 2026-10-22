//! The bucket lease: one writer per backup prefix.
//!
//! The first half drives [`BucketLease`] directly; the second half runs the
//! worker against a bucket another instance holds, loses its lease to, or is
//! partitioned from.

use backup::lease::{lease_key, LeaseRecord};
use backup::{BackupConfig, BackupTrigger, BackupWorker, BucketLease, Exclusion, LeaseError};
use mushroomdb::SharedDb;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use object_store::{ObjectStore, PutPayload};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const PREFIX: &str = "pfx";

fn mem() -> Arc<dyn ObjectStore> {
    Arc::new(InMemory::new())
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn parse(s: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).unwrap()
}

/// Write a LEASE as another instance would, renewed `age_s` seconds ago
/// (negative: in the future).
async fn write_foreign(store: &Arc<dyn ObjectStore>, id: &str, age_s: i64, ttl_s: u64) {
    let renewed = time::OffsetDateTime::now_utc() - time::Duration::seconds(age_s);
    let record = LeaseRecord {
        instance_id: id.into(),
        acquired_at: rfc3339(renewed),
        renewed_at: rfc3339(renewed),
        ttl_s,
    };
    store
        .put(
            &ObjectPath::from(lease_key(PREFIX)),
            PutPayload::from(serde_json::to_vec(&record).unwrap()),
        )
        .await
        .unwrap();
}

async fn read_lease(store: &Arc<dyn ObjectStore>) -> Option<LeaseRecord> {
    match store.get(&ObjectPath::from(lease_key(PREFIX))).await {
        Ok(r) => Some(serde_json::from_slice(&r.bytes().await.unwrap()).unwrap()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(e) => panic!("get LEASE: {e}"),
    }
}

// ── The brief's four ─────────────────────────────────────────────────────────

#[tokio::test]
async fn stale_foreign_lease_permits() {
    let store = mem();
    write_foreign(&store, "someone-else", 60, 30).await;
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .expect("a lease two TTLs old is free to take");
    let now = read_lease(&store).await.unwrap();
    assert_eq!(now.instance_id, lease.instance_id());
    assert_eq!(now.ttl_s, 30);
}

#[tokio::test]
async fn fresh_foreign_lease_refuses() {
    let store = mem();
    write_foreign(&store, "someone-else", 1, 30).await;
    match BucketLease::acquire(Arc::clone(&store), PREFIX, 30).await {
        Err(LeaseError::ForeignActive { instance_id }) => assert_eq!(instance_id, "someone-else"),
        Err(e) => panic!("expected ForeignActive, got {e}"),
        Ok(_) => panic!("expected ForeignActive, got a lease"),
    }
    assert_eq!(
        read_lease(&store).await.unwrap().instance_id,
        "someone-else",
        "the holder's lease is untouched"
    );
}

#[tokio::test]
async fn own_lease_renews() {
    let store = mem();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    lease.renew().await.unwrap();
    let r = read_lease(&store).await.unwrap();
    assert_eq!(r.instance_id, lease.instance_id());
    assert!(
        parse(&r.renewed_at) > parse(&r.acquired_at),
        "renewed_at {} is later than acquired_at {}",
        r.renewed_at,
        r.acquired_at
    );
}

#[tokio::test]
async fn release_deletes_lease() {
    let store = mem();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    lease.release().await.unwrap();
    assert!(matches!(
        store.get(&ObjectPath::from(lease_key(PREFIX))).await,
        Err(object_store::Error::NotFound { .. })
    ));
}

// ── Exclusion, skew, takeover ────────────────────────────────────────────────

/// On a store with conditional PUT, contenders that start together get
/// exactly one lease between them: the check and the write are one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_acquirers_get_exactly_one_lease() {
    for round in 0..20 {
        let store = mem();
        if round % 2 == 1 {
            // Half the rounds race over a stale lease instead of an empty key.
            write_foreign(&store, "dead", 600, 30).await;
        }
        let tries: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&store);
                tokio::spawn(async move { BucketLease::acquire(s, PREFIX, 30).await })
            })
            .collect();
        let mut winners = Vec::new();
        for t in tries {
            match t.await.unwrap() {
                Ok(l) => winners.push(l),
                Err(LeaseError::ForeignActive { .. }) => {}
                Err(e) => panic!("round {round}: {e}"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: one lease");
        assert_eq!(winners[0].exclusion(), Exclusion::Conditional);
        assert_eq!(
            read_lease(&store).await.unwrap().instance_id,
            winners[0].instance_id()
        );
    }
}

/// A holder whose lease was taken over learns so at its next renewal, stops
/// writing at once, and does not delete the new holder's lease on release.
#[tokio::test]
async fn a_renewal_after_a_takeover_reports_the_lease_lost() {
    let store = mem();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    assert!(lease.may_write());
    write_foreign(&store, "successor", 0, 30).await;
    match lease.renew().await {
        Err(LeaseError::Lost { instance_id }) => {
            assert_eq!(instance_id.as_deref(), Some("successor"))
        }
        other => panic!("expected Lost, got {other:?}"),
    }
    assert!(lease.is_lost());
    assert!(!lease.may_write(), "a lost lease never writes again");
    assert!(lease.renew().await.is_err(), "and never renews again");
    lease.release().await.unwrap();
    assert_eq!(
        read_lease(&store).await.unwrap().instance_id,
        "successor",
        "release leaves the successor's lease alone"
    );
}

/// Clock skew: a lease stamped in the future by a holder whose clock runs
/// ahead is fresh, not free.
#[tokio::test]
async fn a_future_dated_lease_counts_as_fresh() {
    let store = mem();
    write_foreign(&store, "fast-clock", -20, 30).await;
    assert!(matches!(
        BucketLease::acquire(Arc::clone(&store), PREFIX, 30).await,
        Err(LeaseError::ForeignActive { .. })
    ));
}

/// The holder's own TTL decides when its lease is free, whatever the
/// contender's is: the holder fences its writes by its own TTL.
#[tokio::test]
async fn the_holders_ttl_decides_freshness() {
    let store = mem();
    write_foreign(&store, "long-ttl", 60, 120).await;
    assert!(matches!(
        BucketLease::acquire(Arc::clone(&store), PREFIX, 30).await,
        Err(LeaseError::ForeignActive { .. })
    ));
}

/// A LEASE that does not parse belongs to nobody.
#[tokio::test]
async fn an_unreadable_lease_is_taken_over() {
    let store = mem();
    store
        .put(
            &ObjectPath::from(lease_key(PREFIX)),
            PutPayload::from_static(b"not json"),
        )
        .await
        .unwrap();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    assert_eq!(
        read_lease(&store).await.unwrap().instance_id,
        lease.instance_id()
    );
}

/// The holder stops writing a third of a TTL before anyone else may take the
/// lease, and a successful renewal moves that point on.
#[tokio::test(start_paused = true)]
async fn writes_stop_a_third_of_a_ttl_before_the_lease_can_expire() {
    let store = mem();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(lease.may_write(), "19 s into a 30 s lease");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!lease.may_write(), "21 s: past two thirds, fenced");
    assert!(!lease.is_lost(), "fenced is not lost");
    lease.renew().await.unwrap();
    assert!(lease.may_write(), "a renewal lifts the fence");
}

/// A store with create-if-absent but no compare-and-swap (the local file
/// system): the probe finds the client cannot send If-Match, so the lease is
/// best-effort from the start, and renewal still notices a takeover.
#[tokio::test]
async fn a_store_without_conditional_update_is_best_effort() {
    let bucket = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    assert_eq!(lease.exclusion(), Exclusion::BestEffort);
    lease.renew().await.unwrap();
    lease.renew().await.unwrap();
    write_foreign(&store, "successor", 0, 30).await;
    assert!(matches!(lease.renew().await, Err(LeaseError::Lost { .. })));
}

/// A renewal that landed on the service but whose answer was lost leaves
/// the lease with a version the holder never saw (written here as that
/// renewal would have). The next renewal finds its own id there and carries
/// on: the lease is not lost.
#[tokio::test]
async fn a_renewal_whose_answer_was_lost_is_not_a_lost_lease() {
    let store = mem();
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    write_foreign(&store, lease.instance_id(), 0, 30).await;
    lease.renew().await.expect("the lease is still ours");
    assert!(!lease.is_lost());
    assert!(lease.may_write());
    assert_eq!(
        read_lease(&store).await.unwrap().instance_id,
        lease.instance_id()
    );
}

// ── The worker ───────────────────────────────────────────────────────────────

/// A three-second lease and a short request deadline, so the worker tests
/// run in seconds of real time. Freshness is wall-clock, so these tests do
/// not pause Tokio's clock.
fn fast_config() -> BackupConfig {
    BackupConfig {
        wal_interval_ms: 100,
        manifest_interval_s: 0,
        lease_ttl_s: 3,
        op_timeout_ms: 300,
        shutdown_grace_ms: 2000,
        ..BackupConfig::new(format!("s3://test-bucket/{PREFIX}"))
    }
}

async fn data_keys(store: &Arc<dyn ObjectStore>) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![ObjectPath::from(PREFIX)];
    while let Some(d) = dirs.pop() {
        let r = store.list_with_delimiter(Some(&d)).await.unwrap();
        out.extend(
            r.objects
                .into_iter()
                .map(|o| o.location.to_string())
                .filter(|k| !k.ends_with("/LEASE")),
        );
        dirs.extend(r.common_prefixes);
    }
    out.sort();
    out
}

async fn wait_until<F: std::future::Future<Output = bool>>(
    what: &str,
    limit: Duration,
    mut cond: impl FnMut() -> F,
) {
    let start = tokio::time::Instant::now();
    while !cond().await {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn has_latest(store: &Arc<dyn ObjectStore>) -> bool {
    store
        .head(&ObjectPath::from(format!("{PREFIX}/LATEST")))
        .await
        .is_ok()
}

fn spawn(
    store: &Arc<dyn ObjectStore>,
    db: &SharedDb,
    dir: &std::path::Path,
) -> (watch::Sender<BackupTrigger>, backup::BackupHandle) {
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let h = BackupWorker::spawn(
        fast_config(),
        Arc::clone(store),
        Arc::new(db.clone()),
        dir.to_path_buf(),
        rx,
    )
    .unwrap();
    (tx, h)
}

async fn stop(tx: watch::Sender<BackupTrigger>, h: backup::BackupHandle) {
    tx.send(BackupTrigger::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(10), h.join)
        .await
        .expect("the worker stops")
        .unwrap();
}

fn reason(h: &backup::BackupHandle) -> String {
    h.reason.lock().unwrap().clone().unwrap_or_default()
}

/// A worker that finds a fresh foreign lease uploads nothing and says why;
/// once the holder's lease expires (it died), the worker takes over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_facing_a_fresh_foreign_lease_uploads_nothing_until_it_expires() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    write_foreign(&store, "the-primary", 0, 3).await;
    let (tx, h) = spawn(&store, &db, dir.path());

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(data_keys(&store).await.is_empty(), "nothing uploaded");
    assert!(!h.healthy.load(Ordering::Acquire), "backup is not ok");
    let why = reason(&h);
    assert!(why.contains("the-primary"), "{why}");
    assert_eq!(read_lease(&store).await.unwrap().instance_id, "the-primary");

    // The primary never renews: it died. Within a TTL and a poll the worker
    // holds the lease and uploads.
    wait_until("the takeover", Duration::from_secs(8), || {
        has_latest(&store)
    })
    .await;
    assert_ne!(read_lease(&store).await.unwrap().instance_id, "the-primary");
    wait_until("health", Duration::from_secs(3), || {
        let ok = h.healthy.load(Ordering::Acquire);
        async move { ok }
    })
    .await;
    stop(tx, h).await;
    assert!(read_lease(&store).await.is_none(), "released on shutdown");
}

/// Two workers started on one bucket: one uploads, the other names it and
/// uploads nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_on_one_bucket_one_uploads() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (db1, db2) = (
        SharedDb::open(d1.path()).unwrap(),
        SharedDb::open(d2.path()).unwrap(),
    );
    db1.write().insert_node("A", "one", vec![]).unwrap();
    db2.write().insert_node("A", "two", vec![]).unwrap();
    let store = mem();
    let (t1, h1) = spawn(&store, &db1, d1.path());
    let (t2, h2) = spawn(&store, &db2, d2.path());
    wait_until("a holder", Duration::from_secs(5), || has_latest(&store)).await;
    // Keep both running across several renewals.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let holder = read_lease(&store).await.unwrap().instance_id;
    let (ok1, ok2) = (
        h1.healthy.load(Ordering::Acquire),
        h2.healthy.load(Ordering::Acquire),
    );
    assert!(ok1 ^ ok2, "exactly one is healthy: {ok1} {ok2}");
    let loser = if ok1 { &h2 } else { &h1 };
    assert!(reason(loser).contains(&holder), "{}", reason(loser));
    stop(t1, h1).await;
    stop(t2, h2).await;
}

/// Shutdown deletes the lease, so a successor starts at once rather than a
/// TTL later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_releases_the_lease_and_a_successor_starts_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let (tx, h) = spawn(&store, &db, dir.path());
    wait_until("the first upload", Duration::from_secs(5), || {
        has_latest(&store)
    })
    .await;
    assert!(read_lease(&store).await.is_some());
    stop(tx, h).await;
    assert!(read_lease(&store).await.is_none());

    db.write().insert_node("A", "b", vec![]).unwrap();
    let before = data_keys(&store).await.len();
    let (tx, h) = spawn(&store, &db, dir.path());
    wait_until(
        "the successor's upload",
        Duration::from_millis(1500),
        || {
            let s = Arc::clone(&store);
            async move { data_keys(&s).await.len() > before }
        },
    )
    .await;
    assert!(h.healthy.load(Ordering::Acquire));
    stop(tx, h).await;
}

/// A view of `inner` whose every request can be made to hang: the
/// network to the bucket is gone. Requests made while it is hung time out
/// on the worker's deadline.
struct Partition(Arc<ThrottledStore<Arc<dyn ObjectStore>>>);

impl Partition {
    fn new(inner: &Arc<dyn ObjectStore>) -> Self {
        Self(Arc::new(ThrottledStore::new(
            Arc::clone(inner),
            ThrottleConfig::default(),
        )))
    }

    fn store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.0) as Arc<dyn ObjectStore>
    }

    fn set_down(&self, down: bool) {
        let wait = if down {
            Duration::from_secs(365 * 24 * 3600)
        } else {
            Duration::ZERO
        };
        self.0.config_mut(|c| {
            c.wait_put_per_call = wait;
            c.wait_get_per_call = wait;
            c.wait_list_per_call = wait;
            c.wait_list_with_delimiter_per_call = wait;
            c.wait_delete_per_call = wait;
        });
    }
}

/// Every object but `LEASE`, with its bytes.
async fn data_objects(store: &Arc<dyn ObjectStore>) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for k in data_keys(store).await {
        if let Ok(r) = store.get(&ObjectPath::from(k.as_str())).await {
            out.push((k, r.bytes().await.unwrap().to_vec()));
        }
    }
    out
}

/// A network partition: the holder cannot reach the bucket for longer than
/// its TTL, a contender takes the lease over, and the partition heals. The
/// old holder writes nothing more — not while fenced, not after the heal —
/// and does not delete the new holder's lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partitioned_holder_never_writes_after_a_takeover() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let inner = mem();
    let net = Partition::new(&inner);
    let (tx, h) = spawn(&net.store(), &db, dir.path());
    wait_until("the first upload", Duration::from_secs(5), || {
        has_latest(&inner)
    })
    .await;
    let holder = read_lease(&inner).await.unwrap().instance_id;

    net.set_down(true);
    for i in 0..5 {
        db.write()
            .insert_node("A", &format!("p{i}"), vec![])
            .unwrap();
    }
    // Past the holder's TTL since its last renewal: the lease is free.
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let successor = BucketLease::acquire(Arc::clone(&inner), PREFIX, 30)
        .await
        .expect("the partitioned holder's lease has expired");
    let at_takeover = data_objects(&inner).await;

    net.set_down(false);
    for i in 0..5 {
        db.write()
            .insert_node("A", &format!("q{i}"), vec![])
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        data_objects(&inner).await,
        at_takeover,
        "no data write from the old holder reached the bucket"
    );
    assert_eq!(
        read_lease(&inner).await.unwrap().instance_id,
        successor.instance_id()
    );
    assert!(!h.healthy.load(Ordering::Acquire));
    let why = reason(&h);
    assert!(why.contains(successor.instance_id()), "{why}");
    assert_ne!(holder, successor.instance_id());
    stop(tx, h).await;
    assert_eq!(
        read_lease(&inner).await.unwrap().instance_id,
        successor.instance_id(),
        "the old holder's shutdown leaves the new lease alone"
    );
}

/// The holder can still reach its data but not its lease: every renewal
/// fails while uploads would succeed. Only the holder's own fence stops it,
/// and it must stop before anyone may take the lease — so nothing it writes
/// lands after the takeover. (The lease file is made unreadable on a local
/// bucket; every lease step starts with a read.)
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_that_cannot_renew_stops_writing_before_a_takeover() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let bucket = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let (tx, h) = spawn(&store, &db, dir.path());
    wait_until("the first upload", Duration::from_secs(5), || {
        has_latest(&store)
    })
    .await;
    let lease_file = bucket.path().join(PREFIX).join("LEASE");
    std::fs::set_permissions(&lease_file, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Writes keep coming the whole time, so an unfenced holder would ship a
    // segment every 100 ms.
    let writing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let writer = {
        let (db, writing) = (db.clone(), Arc::clone(&writing));
        tokio::task::spawn_blocking(move || {
            let mut i = 0;
            while writing.load(Ordering::SeqCst) {
                db.write()
                    .insert_node("A", &format!("w{i}"), vec![])
                    .unwrap();
                i += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    // A full TTL after the holder's last renewal, by the contender's clock.
    tokio::time::sleep(Duration::from_millis(3300)).await;
    assert!(!h.healthy.load(Ordering::Acquire));
    assert!(reason(&h).contains("renew"), "{}", reason(&h));
    // The takeover: a successor's lease replaces the file.
    let at_takeover = data_objects(&store).await;
    write_foreign(&store, "successor", 0, 30).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        data_objects(&store).await,
        at_takeover,
        "the old holder fenced itself before the takeover"
    );
    wait_until("the loss to be noticed", Duration::from_secs(5), || {
        let why = reason(&h);
        async move { why.contains("successor") }
    })
    .await;
    writing.store(false, Ordering::SeqCst);
    writer.await.unwrap();
    stop(tx, h).await;
    assert_eq!(read_lease(&store).await.unwrap().instance_id, "successor");
}

/// A renewal that keeps failing — the bucket is unreachable — fences the
/// holder and turns it unhealthy before its lease can expire; a heal before
/// anyone takes over lets it carry on where it was, with nothing lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_that_cannot_renew_turns_unhealthy_then_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let inner = mem();
    let net = Partition::new(&inner);
    let (tx, h) = spawn(&net.store(), &db, dir.path());
    wait_until("the first upload", Duration::from_secs(5), || {
        has_latest(&inner)
    })
    .await;
    let holder = read_lease(&inner).await.unwrap().instance_id;
    net.set_down(true);
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert!(!h.healthy.load(Ordering::Acquire), "{}", reason(&h));
    assert!(reason(&h).contains("renew"), "{}", reason(&h));
    net.set_down(false);
    db.write().insert_node("A", "after", vec![]).unwrap();
    let want = db.read().wal_total_commits().unwrap();
    wait_until("the backlog to ship", Duration::from_secs(10), || {
        let s = Arc::clone(&inner);
        async move {
            let Ok(r) = s.get(&ObjectPath::from(format!("{PREFIX}/LATEST"))).await else {
                return false;
            };
            let key = String::from_utf8(r.bytes().await.unwrap().to_vec()).unwrap();
            let m = s.get(&ObjectPath::from(key)).await.unwrap();
            backup::Manifest::from_json(&m.bytes().await.unwrap())
                .unwrap()
                .commit_seq
                == want
        }
    })
    .await;
    assert_eq!(read_lease(&inner).await.unwrap().instance_id, holder);
    stop(tx, h).await;
}

/// Lease writes that landed though their answers were lost never make the
/// worker wait behind itself. Its first attempt finds a fresh lease naming
/// its own id (an earlier attempt's write) and takes it at once; while it
/// holds the lease, renewals land unseen (the version changes under it) and
/// it carries on. Health never names it as a foreign holder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_lease_answers_never_lock_the_worker_out_behind_itself() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let store = mem();
    let w = BackupWorker::new(
        fast_config(),
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    let id = w.instance_id().to_string();
    write_foreign(&store, &id, 0, 3).await;
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let h = w.start(rx);
    wait_until("the first upload", Duration::from_millis(1500), || {
        has_latest(&store)
    })
    .await;
    let mut reasons = Vec::new();
    for _ in 0..8 {
        write_foreign(&store, &id, 0, 3).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        reasons.push(reason(&h));
    }
    for r in &reasons {
        assert!(!r.contains("held by another instance"), "{r}");
        assert!(!r.contains(&id), "named itself: {r}");
    }
    assert!(h.healthy.load(Ordering::Acquire), "{}", reason(&h));
    assert_eq!(read_lease(&store).await.unwrap().instance_id, id);
    stop(tx, h).await;
}
