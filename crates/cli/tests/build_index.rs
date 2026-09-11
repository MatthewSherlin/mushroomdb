//! `mushroomdb build-index` drives a rule's vector index to completion.
//!
//! A rule created over a corpus larger than one build slice returns before its
//! edges exist and reports progress instead; this command is how an operator
//! finishes the job before traffic arrives.
use cli::{build_index_on, run_build_index};
use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-buildindex-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// One tight cluster of ten vectors per axis, so the derived edge set is
/// exactly the within-cluster pairs.
fn slice_emb(i: usize) -> Value {
    const D: usize = 32;
    let axis = (i / 10) % D;
    let mut xs = vec![0.0f64; D];
    xs[axis] = 1.0;
    xs[(axis + 1) % D] = (i % 10) as f64 * 0.001;
    Value::List(xs.into_iter().map(Value::Float).collect())
}

fn sim_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.9,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn seed(dir: &std::path::Path, n: usize, batch: Option<usize>) -> GraphDb<core_api::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    for i in 0..n {
        db.insert_node("V", &format!("v{i}"), vec![("emb".into(), slice_emb(i))])
            .unwrap();
    }
    db.set_hnsw_build_batch(batch);
    db.create_rule(sim_rule()).unwrap();
    if batch.is_some() {
        assert!(
            !db.builds_in_progress().is_empty(),
            "the fixture must leave a build outstanding"
        );
    }
    db
}

#[test]
fn build_index_pumps_to_completion_and_reports_each_slice() {
    let dir = tmp("pump");
    // The handle that created the rule is the one that holds the outstanding
    // build: reopening would replay `CreateRule` at the production slice size.
    let mut db = seed(&dir, 300, Some(64));

    let out = build_index_on(&mut db, None).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.iter().any(|l| l.starts_with("building sim: ")),
        "one progress line per slice; got {out:?}"
    );
    assert_eq!(
        lines.last(),
        Some(&"built sim: 300 vectors"),
        "the last line announces completion; got {out:?}"
    );

    // The backfill ran: the rule owns its edges now.
    assert!(db.builds_in_progress().is_empty());
    let n = db.neighbors("v0", "SIM", Direction::Out).unwrap();
    assert!(
        n.contains(&"v1".to_string()),
        "v0 must link to its cluster; got {n:?}"
    );
}

#[test]
fn build_index_says_so_when_there_is_nothing_to_build() {
    let dir = tmp("idle");
    drop(seed(&dir, 20, None));

    assert_eq!(run_build_index(&dir, None).unwrap(), "nothing to build\n");
    assert_eq!(
        run_build_index(&dir, Some("sim")).unwrap(),
        "nothing to build for rule \"sim\"\n"
    );
}

/// `--rule` narrows the report to one rule; a name that is not building is not
/// an error, it is nothing to do.
#[test]
fn build_index_filters_by_rule() {
    let dir = tmp("filter");
    let mut db = seed(&dir, 300, Some(64));

    assert_eq!(
        build_index_on(&mut db, Some("other")).unwrap(),
        "nothing to build for rule \"other\"\n"
    );
    let out = build_index_on(&mut db, Some("sim")).unwrap();
    assert!(out.ends_with("built sim: 300 vectors\n"), "got {out:?}");
}
