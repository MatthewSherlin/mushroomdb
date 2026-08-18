use core_api::SharedDb;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn clone_shares_state() {
    let dir = tmp("shared-clone");
    let a = SharedDb::open(&dir).unwrap();
    let b = a.clone();
    a.write().insert_node("N", "k", vec![]).unwrap();
    assert!(b.read().has_node("k"));
    assert_eq!(b.read().stats().nodes_live, 1);
}

#[test]
fn concurrent_readers_sum_stats_while_writer_inserts() {
    let dir = tmp("shared-conc");
    let db = SharedDb::open(&dir).unwrap();
    const N_READERS: usize = 8;
    const N_INSERTS: usize = 50;

    let start = Arc::new(Barrier::new(N_READERS + 1));
    let writer_done = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(AtomicUsize::new(0));
    let sum = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..N_READERS)
        .map(|_| {
            let db = db.clone();
            let start = Arc::clone(&start);
            let writer_done = Arc::clone(&writer_done);
            let samples = Arc::clone(&samples);
            let sum = Arc::clone(&sum);
            thread::spawn(move || {
                start.wait();
                loop {
                    let n = db.read().stats().nodes_live;
                    samples.fetch_add(1, Ordering::Relaxed);
                    sum.fetch_add(n, Ordering::Relaxed);
                    if writer_done.load(Ordering::Acquire) {
                        let n = db.read().stats().nodes_live;
                        samples.fetch_add(1, Ordering::Relaxed);
                        sum.fetch_add(n, Ordering::Relaxed);
                        break;
                    }
                }
            })
        })
        .collect();

    start.wait();
    for i in 0..N_INSERTS {
        db.write()
            .insert_node("N", &format!("n{i}"), vec![])
            .unwrap();
    }
    writer_done.store(true, Ordering::Release);

    for h in readers {
        h.join().expect("reader thread panicked");
    }

    let stats = db.read().stats();
    assert_eq!(stats.nodes_live, N_INSERTS);
    assert_eq!(stats.nodes_tombstoned, 0);
    assert_eq!(stats.edges, 0);
    assert_eq!(db.read().node_count(), N_INSERTS);
    assert!(samples.load(Ordering::Relaxed) > 0);
    // Each reader adds at least the post-write snapshot (N_INSERTS).
    assert!(sum.load(Ordering::Relaxed) >= N_READERS * N_INSERTS);
}

#[test]
fn reader_during_write_observes_before_or_after_only() {
    let dir = tmp("shared-torn");
    let db = SharedDb::open(&dir).unwrap();
    const BATCH: usize = 40;
    const N_READERS: usize = 4;

    let start = Arc::new(Barrier::new(N_READERS + 1));
    let writer_done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..N_READERS)
        .map(|_| {
            let db = db.clone();
            let start = Arc::clone(&start);
            let writer_done = Arc::clone(&writer_done);
            thread::spawn(move || {
                let mut seen = Vec::new();
                start.wait();
                loop {
                    let n = db.read().stats().nodes_live;
                    assert!(
                        n == 0 || n == BATCH,
                        "torn read through SharedDb API: nodes_live={n}"
                    );
                    seen.push(n);
                    if writer_done.load(Ordering::Acquire) {
                        let n = db.read().stats().nodes_live;
                        assert!(
                            n == 0 || n == BATCH,
                            "torn read through SharedDb API: nodes_live={n}"
                        );
                        seen.push(n);
                        break;
                    }
                }
                seen
            })
        })
        .collect();

    start.wait();
    {
        let mut w = db.write();
        for i in 0..BATCH {
            w.insert_node("N", &format!("t{i}"), vec![]).unwrap();
        }
    }
    writer_done.store(true, Ordering::Release);

    for h in readers {
        let seen = h.join().expect("reader thread panicked");
        assert!(seen.iter().all(|&n| n == 0 || n == BATCH));
    }
    assert_eq!(db.read().stats().nodes_live, BATCH);
}
