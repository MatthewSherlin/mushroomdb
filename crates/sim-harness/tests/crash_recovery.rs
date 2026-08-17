use core_api::{GraphDb, Value};
use core_storage::fs::Fs;
use sim_harness::SimFs;

/// The deterministic workload: same ops every run.
fn workload<F: Fs>(db: &mut GraphDb<F>) -> core_api::Result<()> {
    for i in 0..20 {
        db.insert_node("N", &format!("n{i}"), vec![("i".into(), Value::Int(i))])?;
        if i > 0 {
            db.insert_edge("E", &format!("n{}", i - 1), &format!("n{i}"))?;
        }
        if i == 10 {
            db.snapshot()?;
        }
    }
    Ok(())
}

#[test]
fn recovery_is_consistent_at_every_crash_offset() {
    // Run to completion to measure total bytes appended.
    let total = {
        let mut db = GraphDb::open_with(SimFs::new()).unwrap();
        workload(&mut db).unwrap();
        db.fs_total_appended()
    };
    assert!(total > 0);

    for crash_at in 0..=total {
        let mut db = GraphDb::open_with(SimFs::with_crash_after(crash_at)).unwrap();
        let _ = workload(&mut db); // errors expected once the crash fires
        let survivor = db.into_fs().surviving_state();

        // Invariant 1: recovery never panics or reports corruption.
        let recovered = GraphDb::open_with(survivor).unwrap();

        // Invariant 2: recovered state is internally consistent.
        let n = recovered.node_count() as i64;
        for i in 0..n {
            assert!(
                recovered.has_node(&format!("n{i}")),
                "crash_at={crash_at}: missing n{i}"
            );
            assert_eq!(
                recovered.get_prop(&format!("n{i}"), "i"),
                Some(&Value::Int(i)),
                "crash_at={crash_at}: node exists but its logged props are missing"
            );
        }
        // Edges only ever connect existing, consecutive nodes.
        assert!(
            recovered.edge_count() <= (n.max(1) - 1) as u64,
            "crash_at={crash_at}"
        );
    }
}
