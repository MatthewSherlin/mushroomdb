use super::*;
use crate::lease::{lease_key, ProbeControl};
use object_store::memory::InMemory;
use std::sync::atomic::AtomicUsize;

const PREFIX: &str = "pending-lease";

fn config() -> BackupConfig {
    BackupConfig {
        wal_interval_ms: 100,
        manifest_interval_s: 0,
        lease_ttl_s: 30,
        op_timeout_ms: 300,
        shutdown_grace_ms: 1000,
        ..BackupConfig::new(format!("s3://bucket/{PREFIX}"))
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    db: SharedDb,
    store: Arc<dyn ObjectStore>,
    control: Arc<ProbeControl>,
    worker: BackupWorker,
}

fn setup(probe: usize, config: BackupConfig) -> Fixture {
    setup_with_store(probe, config, Arc::new(InMemory::new()))
}

fn setup_with_store(probe: usize, config: BackupConfig, store: Arc<dyn ObjectStore>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path()).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let control = Arc::new(ProbeControl {
        block_probe: probe,
        acquisitions: AtomicUsize::new(0),
        ..ProbeControl::default()
    });
    let mut worker = BackupWorker::new(
        config,
        Arc::clone(&store),
        Arc::new(db.clone()),
        dir.path().to_path_buf(),
    )
    .unwrap();
    worker.watch.probe_control = Some(Arc::clone(&control));
    Fixture {
        dir,
        db,
        store,
        control,
        worker,
    }
}

async fn bytes(store: &Arc<dyn ObjectStore>, key: &str) -> Option<Vec<u8>> {
    match store.get(&ObjectPath::from(key)).await {
        Ok(result) => Some(result.bytes().await.unwrap().to_vec()),
        Err(object_store::Error::NotFound { .. }) => None,
        Err(error) => panic!("{key}: {error}"),
    }
}

async fn assert_final_manifest(store: &Arc<dyn ObjectStore>, db: &SharedDb) {
    assert!(bytes(store, &lease_key(PREFIX)).await.is_none());
    let latest = bytes(store, &latest_key(PREFIX))
        .await
        .expect("shutdown publishes the final restore point");
    let key = String::from_utf8(latest).unwrap();
    let manifest = Manifest::from_json(&bytes(store, &key).await.unwrap()).unwrap();
    assert_eq!(manifest.commit_seq, db.read().wal_total_commits().unwrap());
}

async fn assert_no_data(store: &Arc<dyn ObjectStore>) {
    let listed = store
        .list_with_delimiter(Some(&ObjectPath::from(PREFIX)))
        .await
        .unwrap();
    assert!(
        listed.common_prefixes.is_empty(),
        "no data directories before validation"
    );
    assert!(listed
        .objects
        .iter()
        .all(|object| object.location.as_ref() == lease_key(PREFIX)));
}

#[tokio::test]
async fn shutdown_finishes_each_pending_probe_before_flushing() {
    for probe in [0, 1] {
        let Fixture {
            dir: _dir,
            db,
            store,
            control,
            worker,
        } = setup(probe, config());
        let (tx, rx) = watch::channel(BackupTrigger::Idle);
        let handle = worker.start(rx);
        control.entered.notified().await;
        assert!(bytes(&store, &lease_key(PREFIX)).await.is_some());
        assert!(bytes(&store, &latest_key(PREFIX)).await.is_none());
        assert_no_data(&store).await;
        assert!(!handle.lease_held.load(Ordering::Acquire));
        db.write().insert_node("A", "last-acked", vec![]).unwrap();
        let asked = Instant::now();
        tx.send(BackupTrigger::Shutdown).unwrap();
        control.shutdown.notified().await;
        control.resume.notify_one();
        handle.join.await.unwrap();
        eprintln!(
            "healthy pending probe {probe}: shutdown {:?}",
            asked.elapsed()
        );
        assert!(asked.elapsed() < config().shutdown_budget());
        assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(control.probes.load(Ordering::SeqCst), 2);
        assert_final_manifest(&store, &db).await;
    }
}

#[tokio::test]
async fn shutdown_cancels_a_blocked_probe_before_owner_cleanup() {
    let mut settings = config();
    settings.shutdown_grace_ms = 100;
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        worker,
    } = setup(0, settings.clone());
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    let asked = Instant::now();
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    eprintln!("blocked pending probe: shutdown {:?}", asked.elapsed());
    assert!(asked.elapsed() < settings.shutdown_budget());
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
    assert!(bytes(&store, &latest_key(PREFIX)).await.is_none());
    control.resume.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(control.probes.load(Ordering::SeqCst), 0);
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
}

#[tokio::test]
async fn shutdown_recovers_own_record_after_the_original_attempt_times_out() {
    let mut settings = config();
    settings.op_timeout_ms = 25;
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        worker,
    } = setup(0, settings);
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    control.failed.notified().await;
    assert_eq!(handle.error_count.load(Ordering::Acquire), 1);
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_some());
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
    assert_eq!(
        control.acquisitions.load(Ordering::SeqCst),
        1,
        "no fresh attempt during shutdown"
    );
    control.resume.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(control.probes.load(Ordering::SeqCst), 0);
    assert_no_data(&store).await;
}

#[tokio::test]
async fn fallback_cleanup_leaves_foreign_absent_and_malformed_records_unchanged() {
    for replacement in [
        Some(b"not-json".to_vec()),
        None,
        Some(
            serde_json::to_vec(&crate::lease::LeaseRecord {
                instance_id: "successor".into(),
                acquired_at: now_rfc3339(),
                renewed_at: now_rfc3339(),
                ttl_s: 30,
            })
            .unwrap(),
        ),
    ] {
        let mut settings = config();
        settings.shutdown_grace_ms = 50;
        let Fixture {
            dir: _dir,
            db: _db,
            store,
            control,
            worker,
        } = setup(0, settings);
        let (tx, rx) = watch::channel(BackupTrigger::Idle);
        let handle = worker.start(rx);
        control.entered.notified().await;
        let key = ObjectPath::from(lease_key(PREFIX));
        match &replacement {
            Some(value) => {
                store.put(&key, value.clone().into()).await.unwrap();
            }
            None => {
                store.delete(&key).await.unwrap();
            }
        }
        tx.send(BackupTrigger::Shutdown).unwrap();
        handle.join.await.unwrap();
        assert_eq!(bytes(&store, key.as_ref()).await, replacement);
        assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
        control.resume.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(control.probes.load(Ordering::SeqCst), 0);
        assert_no_data(&store).await;
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_probe_and_black_holed_cleanup_share_the_original_shutdown_budget() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let throttled = Arc::new(ThrottledStore::new(
        Arc::clone(&inner),
        ThrottleConfig::default(),
    ));
    let mut settings = config();
    settings.shutdown_grace_ms = 100;
    settings.op_timeout_ms = 100;
    let Fixture {
        dir: _dir,
        db: _db,
        store: _,
        control,
        worker,
    } = setup_with_store(0, settings, throttled.clone());
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    throttled.config_mut(|config| config.wait_get_per_call = Duration::from_secs(86400));
    let asked = Instant::now();
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    assert!(
        asked.elapsed() <= Duration::from_millis(200),
        "{:?}",
        asked.elapsed()
    );
    assert!(
        bytes(&inner, &lease_key(PREFIX)).await.is_some(),
        "unreachable cleanup leaves lease to TTL"
    );
    control.resume.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(control.probes.load(Ordering::SeqCst), 0);
    assert_no_data(&inner).await;
}

#[tokio::test]
async fn channel_closure_and_simultaneous_probe_completion_finish_exactly_one_attempt() {
    for close_channel in [false, true] {
        for _ in 0..4 {
            let Fixture {
                dir: _dir,
                db,
                store,
                control,
                worker,
            } = setup(1, config());
            let (tx, rx) = watch::channel(BackupTrigger::Idle);
            let handle = worker.start(rx);
            control.entered.notified().await;
            if close_channel {
                drop(tx);
            } else {
                tx.send(BackupTrigger::Shutdown).unwrap();
            }
            // Both validation completion and shutdown can now be ready in the
            // same select poll, without a scheduler-delay guess.
            control.resume.notify_one();
            handle.join.await.unwrap();
            assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
            assert_eq!(control.probes.load(Ordering::SeqCst), 2);
            assert_final_manifest(&store, &db).await;
        }
    }
}

#[tokio::test]
async fn shutdown_after_a_probe_accepts_a_false_precondition_never_uploads() {
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        worker,
    } = setup(0, config());
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    // Make the deliberately false create probe succeed, exercising the
    // real failed-capability outcome and owner-record cleanup path.
    store
        .delete(&ObjectPath::from(lease_key(PREFIX)))
        .await
        .unwrap();
    tx.send(BackupTrigger::Shutdown).unwrap();
    control.shutdown.notified().await;
    control.resume.notify_one();
    handle.join.await.unwrap();
    assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(control.probes.load(Ordering::SeqCst), 1);
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
    assert_no_data(&store).await;
}

#[tokio::test]
async fn shutdown_preserves_unsupported_conditional_update_behavior() {
    let bucket = tempfile::tempdir().unwrap();
    let store: Arc<dyn ObjectStore> =
        Arc::new(object_store::local::LocalFileSystem::new_with_prefix(bucket.path()).unwrap());
    let Fixture {
        dir: _dir,
        db,
        store,
        control,
        worker,
    } = setup_with_store(1, config(), store);
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    tx.send(BackupTrigger::Shutdown).unwrap();
    control.shutdown.notified().await;
    control.resume.notify_one();
    handle.join.await.unwrap();
    assert_eq!(control.probes.load(Ordering::SeqCst), 2);
    assert_final_manifest(&store, &db).await;
}

#[tokio::test(start_paused = true)]
async fn retries_preserve_suspect_foreign_lease_observation_time() {
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control: _,
        mut worker,
    } = setup(0, config());
    worker.watch.probe_control = None;
    let future = time::OffsetDateTime::now_utc() + time::Duration::days(1);
    let stamp = future
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let record = crate::lease::LeaseRecord {
        instance_id: "bad-clock".into(),
        acquired_at: stamp.clone(),
        renewed_at: stamp,
        ttl_s: 30,
    };
    store
        .put(
            &ObjectPath::from(lease_key(PREFIX)),
            serde_json::to_vec(&record).unwrap().into(),
        )
        .await
        .unwrap();
    assert!(worker.take_lease().await.is_some());
    tokio::time::advance(Duration::from_secs(31)).await;
    assert!(
        worker.take_lease().await.is_none(),
        "same suspect record observed for a TTL"
    );
    assert!(worker.holds_lease());
    worker.release_lease().await;
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn delayed_probe_does_not_refresh_the_acquisition_write_fence() {
    let mut settings = config();
    settings.lease_ttl_s = 1;
    settings.op_timeout_ms = 1000;
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        worker,
    } = setup(0, settings);
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    tokio::time::advance(Duration::from_millis(800)).await;
    tx.send(BackupTrigger::Shutdown).unwrap();
    control.shutdown.notified().await;
    control.resume.notify_one();
    handle.join.await.unwrap();
    assert_eq!(control.probes.load(Ordering::SeqCst), 2);
    if bytes(&store, &latest_key(PREFIX)).await.is_some() {
        assert!(
            control.renewals.load(Ordering::SeqCst) > 0,
            "only a real safe renewal can lift the old fence"
        );
    } else {
        assert_no_data(&store).await;
    }
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_expired_pending_lease_puts_no_data_before_a_real_renewal() {
    let mut settings = config();
    settings.lease_ttl_s = 1;
    settings.op_timeout_ms = 1000;
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        mut worker,
    } = setup(0, settings);
    {
        let taking = worker.take_lease();
        tokio::pin!(taking);
        tokio::select! {
            _ = control.entered.notified() => {}
            result = &mut taking => panic!("the real probe must wait: {result:?}"),
        }
    }
    tokio::time::advance(Duration::from_millis(800)).await;
    control.resume.notify_one();
    assert!(worker.finish_lease_attempt(true).await.is_none());
    // No await between adoption returning and aborting this fixture's newly
    // spawned renewer: the current-thread runtime cannot renew it first.
    let renewer = worker.renewer.take().expect("adoption starts a renewer");
    renewer.abort();
    let _ = renewer.await;
    assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(control.probes.load(Ordering::SeqCst), 2);
    assert_eq!(control.renewals.load(Ordering::SeqCst), 0);

    let key = format!("{PREFIX}/fence-order-data");
    let payload = PutPayload::from(vec![1, 2, 3]);
    let denied = worker
        .put_mode(&key, payload.clone(), PutMode::Create)
        .await;
    assert!(
        matches!(denied, Err(BackupError::Lease(_))),
        "expired adoption must fence the first data PUT: {denied:?}"
    );
    assert!(bytes(&store, &key).await.is_none());
    assert_no_data(&store).await;

    worker.lease.as_ref().unwrap().renew().await.unwrap();
    assert_eq!(control.renewals.load(Ordering::SeqCst), 1);
    worker
        .put_mode(&key, payload, PutMode::Create)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes(&store, &key).await, Some(vec![1, 2, 3]));
    worker.release_lease().await;
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
    assert_eq!(control.renewals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn already_adopted_boot_lease_flushes_without_a_new_attempt() {
    let Fixture {
        dir: _dir,
        db,
        store,
        control,
        mut worker,
    } = setup(0, config());
    let lease = BucketLease::acquire(Arc::clone(&store), PREFIX, 30)
        .await
        .unwrap();
    worker.adopt_lease(Arc::new(lease));
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    assert_eq!(control.acquisitions.load(Ordering::SeqCst), 0);
    assert_final_manifest(&store, &db).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_does_not_restart_the_original_acquisition_deadline() {
    let mut settings = config();
    settings.op_timeout_ms = 100;
    let Fixture {
        dir: _dir,
        db: _db,
        store,
        control,
        worker,
    } = setup(0, settings);
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    tokio::time::advance(Duration::from_millis(300)).await;
    let asked = Instant::now();
    tx.send(BackupTrigger::Shutdown).unwrap();
    handle.join.await.unwrap();
    assert!(
        asked.elapsed() <= Duration::from_millis(100),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(control.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(control.probes.load(Ordering::SeqCst), 0);
    assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
    assert_no_data(&store).await;
}

#[tokio::test(start_paused = true)]
async fn pending_validation_and_flush_share_one_absolute_grace() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let throttled = Arc::new(ThrottledStore::new(
        Arc::clone(&inner),
        ThrottleConfig::default(),
    ));
    let mut settings = config();
    settings.shutdown_grace_ms = 200;
    settings.op_timeout_ms = 5000;
    let Fixture {
        dir: _dir,
        db: _db,
        store: _,
        control,
        worker,
    } = setup_with_store(1, settings, throttled.clone());
    let (tx, rx) = watch::channel(BackupTrigger::Idle);
    let handle = worker.start(rx);
    control.entered.notified().await;
    // Remaining validation takes 120 ms, and an upload takes another 120 ms.
    // A separately restarted flush grace would let that upload complete.
    throttled.config_mut(|config| config.wait_put_per_call = Duration::from_millis(120));
    let asked = Instant::now();
    tx.send(BackupTrigger::Shutdown).unwrap();
    control.shutdown.notified().await;
    control.resume.notify_one();
    handle.join.await.unwrap();
    eprintln!("shared pending/flush grace: shutdown {:?}", asked.elapsed());
    assert!(
        asked.elapsed() <= Duration::from_millis(200),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(control.probes.load(Ordering::SeqCst), 2);
    assert!(bytes(&inner, &lease_key(PREFIX)).await.is_none());
    assert_no_data(&inner).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_ready_with_the_first_tick_does_not_start_an_acquisition() {
    for close_channel in [false, true] {
        for _ in 0..32 {
            let Fixture {
                dir: _dir,
                db: _db,
                store,
                control,
                worker,
            } = setup(0, config());
            let (tx, rx) = watch::channel(BackupTrigger::Idle);
            // The initial zero-delay tick and shutdown are both ready on
            // the worker's very first select poll.
            if close_channel {
                drop(tx);
            } else {
                tx.send(BackupTrigger::Shutdown).unwrap();
            }
            worker.run(rx).await;
            assert_eq!(
                control.acquisitions.load(Ordering::SeqCst),
                0,
                "an already-observable shutdown must not start another attempt"
            );
            assert_no_data(&store).await;
            assert!(bytes(&store, &lease_key(PREFIX)).await.is_none());
        }
    }
}
