//! A snapshot round trip per rule kind (v0.6.5 spec §9).
//!
//! The v0.5.x data-loss bug (CHANGELOG v0.6.0, "Fixed in this release"): a
//! via-hop rule read topology from the write overlay only, so a rule
//! evaluated on a snapshot-opened store saw no via edges and retracted the
//! ones already derived. Fixed at the root in v0.6.0. `via_rebuild.rs:304`,
//! `rules.rs:1086` and `chaining.rs:136` cover the two via-hop shapes that
//! broke, but nothing exercised the other seven `Predicate` variants
//! systematically, and nothing covered *retraction after reopen* — not "do
//! the edges come back" but "does a write after the reopen retract the right
//! ones and only those", which is the half that actually lost data.
//!
//! One helper, ten cases — one per `Predicate` variant at
//! `crates/core-rules/src/def.rs:85-114`, plus the via-hop shape and a
//! two-rule chain. Each case: seed → assert E₀ → snapshot → reopen → assert
//! E₀ (still, including provenance) → mutate → assert E₀ \ R, where R is a
//! non-empty, strict subset of E₀.
use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "snapshot-rules-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn strs(v: &[&str]) -> Value {
    Value::List(v.iter().map(|s| Value::Str((*s).into())).collect())
}

fn floats(v: &[f64]) -> Value {
    Value::List(v.iter().copied().map(Value::Float).collect())
}

/// All (src, dst) pairs of derived or manual edges of `edge_type`, sorted.
fn edges_of(db: &Db, edge_type: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = db
        .all_edges_for_export()
        .into_iter()
        .filter(|e| e.edge_type == edge_type)
        .map(|e| (e.src, e.dst))
        .collect();
    v.sort();
    v
}

fn owned_sorted(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = pairs
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    v.sort();
    v
}

/// Seed → assert E₀ → snapshot → reopen → assert E₀ → mutate → assert E₀ \ R.
///
/// Step 5 (post-mutation) is the one the 0.5.x bug would have failed: not "do
/// the edges come back" but "does a write after the reopen retract the right
/// ones and only those".
///
/// `expect_after` must always be a strict, non-empty subset of
/// `expect_before` — a rule that retracts everything and a rule that
/// retracts nothing both fail this helper before the store is even touched.
#[allow(clippy::too_many_arguments)]
fn round_trip(
    name: &str,
    edge_type: &str,
    rule_name: &str,
    rules: Vec<RuleDef>,
    seed: impl Fn(&mut Db),
    expect_before: &[(&str, &str)],
    mutate: impl Fn(&mut Db),
    expect_after: &[(&str, &str)],
) {
    let before = owned_sorted(expect_before);
    let after = owned_sorted(expect_after);
    assert!(!after.is_empty(), "{name}: expect_after must be non-empty");
    assert!(
        after.len() < before.len() && after.iter().all(|e| before.contains(e)),
        "{name}: expect_after ({after:?}) must be a strict, non-empty subset of \
         expect_before ({before:?})"
    );

    let dir = tmp(name);
    {
        let mut db: Db = GraphDb::open(&dir).unwrap();
        for r in rules.clone() {
            db.create_rule(r).unwrap();
        }
        seed(&mut db);
        assert_eq!(
            edges_of(&db, edge_type),
            before,
            "{name}: seed must produce E0 before any snapshot"
        );
        db.snapshot().unwrap();
    }

    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        edges_of(&db, edge_type),
        before,
        "{name}: derived edges must survive snapshot + reopen (the 0.5.x data-loss shape)"
    );
    let ev = db.explain(&before[0].0, &before[0].1).unwrap();
    assert!(
        ev.iter().any(|e| e.rule == rule_name),
        "{name}: provenance must survive the reopen, not just topology (got {ev:?})"
    );

    mutate(&mut db);
    assert_eq!(
        edges_of(&db, edge_type),
        after,
        "{name}: exactly the edges whose predicate stopped holding are retracted"
    );
}

// ---------------------------------------------------------------------------
// 1. KeyMatch — clear the key field on one source.
// ---------------------------------------------------------------------------
#[test]
fn key_match_round_trip() {
    round_trip(
        "key-match",
        "WORKS_AT",
        "works_at",
        vec![RuleDef {
            name: "works_at".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::KeyMatch {
                field: "org_id".into(),
            },
            edge_type: "WORKS_AT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node("Org", "o1", vec![]).unwrap();
            db.insert_node("Org", "o2", vec![]).unwrap();
            db.insert_node(
                "Person",
                "p1",
                vec![("org_id".into(), Value::Str("o1".into()))],
            )
            .unwrap();
            db.insert_node(
                "Person",
                "p2",
                vec![("org_id".into(), Value::Str("o2".into()))],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o2")],
        |db| {
            // Clear the key field on p1: an empty string names no live node.
            db.set_prop("p1", "org_id", Value::Str("".into())).unwrap();
        },
        &[("p2", "o2")],
    );
}

// ---------------------------------------------------------------------------
// 2. FieldEqual — change the field on one source.
// ---------------------------------------------------------------------------
#[test]
fn field_equal_round_trip() {
    round_trip(
        "field-equal",
        "FIT",
        "fit",
        vec![RuleDef {
            name: "fit".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::FieldEqual {
                field: "industry".into(),
            },
            edge_type: "FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node(
                "Org",
                "o1",
                vec![("industry".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node(
                "Person",
                "p1",
                vec![("industry".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node(
                "Person",
                "p2",
                vec![("industry".into(), Value::Str("tech".into()))],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            db.set_prop("p1", "industry", Value::Str("law".into()))
                .unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 3. Overlap — drop list elements on one source below `min`.
// ---------------------------------------------------------------------------
#[test]
fn overlap_round_trip() {
    round_trip(
        "overlap",
        "OVERLAPS",
        "overlaps",
        vec![RuleDef {
            name: "overlaps".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.6,
            },
            edge_type: "OVERLAPS".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node("Org", "o1", vec![("tags".into(), strs(&["a", "b"]))])
                .unwrap();
            // jaccard([a,b],[a,b]) = 1.0 >= 0.6
            db.insert_node("Person", "p1", vec![("tags".into(), strs(&["a", "b"]))])
                .unwrap();
            // jaccard([a,b,c],[a,b]) = 2/3 >= 0.6
            db.insert_node(
                "Person",
                "p2",
                vec![("tags".into(), strs(&["a", "b", "c"]))],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // jaccard([a],[a,b]) = 1/2 = 0.5 < 0.6
            db.set_prop("p1", "tags", strs(&["a"])).unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 4. NumericWithin — move one source outside the tolerance.
// ---------------------------------------------------------------------------
#[test]
fn numeric_within_round_trip() {
    round_trip(
        "numeric-within",
        "SAME_ERA",
        "same_era",
        vec![RuleDef {
            name: "same_era".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::NumericWithin {
                field: "year".into(),
                tolerance: 2.0,
            },
            edge_type: "SAME_ERA".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node("Org", "o1", vec![("year".into(), Value::Float(2000.0))])
                .unwrap();
            db.insert_node("Person", "p1", vec![("year".into(), Value::Float(2000.0))])
                .unwrap();
            db.insert_node("Person", "p2", vec![("year".into(), Value::Float(2001.0))])
                .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // delta = 10 > tolerance 2
            db.set_prop("p1", "year", Value::Float(2010.0)).unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 5. GeoRadius — move one source outside the radius.
// ---------------------------------------------------------------------------
#[test]
fn geo_radius_round_trip() {
    round_trip(
        "geo-radius",
        "NEARBY",
        "nearby",
        vec![RuleDef {
            name: "nearby".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            edge_type: "NEARBY".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            // Paris.
            db.insert_node(
                "Org",
                "o1",
                vec![("loc".into(), floats(&[48.8566, 2.3522]))],
            )
            .unwrap();
            // Same spot: distance 0.
            db.insert_node(
                "Person",
                "p1",
                vec![("loc".into(), floats(&[48.8566, 2.3522]))],
            )
            .unwrap();
            // London: ~343.5 km from Paris, inside 400 km.
            db.insert_node(
                "Person",
                "p2",
                vec![("loc".into(), floats(&[51.5074, -0.1278]))],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // Sydney: >16,000 km from Paris, well outside 400 km.
            db.set_prop("p1", "loc", floats(&[-33.8688, 151.2093]))
                .unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 6. VectorSimilar — rewrite one source's vector below `min` (8 dims, fixed).
// ---------------------------------------------------------------------------
#[test]
fn vector_similar_round_trip() {
    round_trip(
        "vector-similar",
        "SIMILAR",
        "similar_vec",
        vec![RuleDef {
            name: "similar_vec".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "SIMILAR".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node(
                "Org",
                "o1",
                vec![(
                    "emb".into(),
                    floats(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                )],
            )
            .unwrap();
            // cosine(p1, o1) = 1.0 >= 0.9
            db.insert_node(
                "Person",
                "p1",
                vec![(
                    "emb".into(),
                    floats(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                )],
            )
            .unwrap();
            // cosine(p2, o1) = 0.9/sqrt(0.82) ~= 0.9939 >= 0.9
            db.insert_node(
                "Person",
                "p2",
                vec![(
                    "emb".into(),
                    floats(&[0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                )],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // Orthogonal to o1: cosine = 0.0 < 0.9.
            db.set_prop(
                "p1",
                "emb",
                floats(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            )
            .unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 7. All([FieldEqual, Overlap]) — break the second conjunct only.
// ---------------------------------------------------------------------------
#[test]
fn all_round_trip() {
    round_trip(
        "all",
        "ALL_FIT",
        "all_fit",
        vec![RuleDef {
            name: "all_fit".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::All(vec![
                Predicate::FieldEqual {
                    field: "industry".into(),
                },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.6,
                },
            ]),
            edge_type: "ALL_FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node(
                "Org",
                "o1",
                vec![
                    ("industry".into(), Value::Str("tech".into())),
                    ("tags".into(), strs(&["a", "b"])),
                ],
            )
            .unwrap();
            // Both conjuncts hold: industry equal, jaccard([a,b],[a,b]) = 1.0.
            db.insert_node(
                "Person",
                "p1",
                vec![
                    ("industry".into(), Value::Str("tech".into())),
                    ("tags".into(), strs(&["a", "b"])),
                ],
            )
            .unwrap();
            // Both conjuncts hold: industry equal, jaccard([a,b,c],[a,b]) = 2/3.
            db.insert_node(
                "Person",
                "p2",
                vec![
                    ("industry".into(), Value::Str("tech".into())),
                    ("tags".into(), strs(&["a", "b", "c"])),
                ],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // industry ("tech") is untouched — only the Overlap conjunct breaks:
            // jaccard([a],[a,b]) = 0.5 < 0.6.
            db.set_prop("p1", "tags", strs(&["a"])).unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 8. Any([FieldEqual, Overlap]) — break BOTH disjuncts on one source.
// ---------------------------------------------------------------------------
#[test]
fn any_round_trip() {
    round_trip(
        "any",
        "ANY_FIT",
        "any_fit",
        vec![RuleDef {
            name: "any_fit".into(),
            src_label: "Person".into(),
            dst_label: "Org".into(),
            predicate: Predicate::Any(vec![
                Predicate::FieldEqual {
                    field: "industry".into(),
                },
                Predicate::Overlap {
                    field: "tags".into(),
                    min: 0.6,
                },
            ]),
            edge_type: "ANY_FIT".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        }],
        |db| {
            db.insert_node(
                "Org",
                "o1",
                vec![
                    ("industry".into(), Value::Str("tech".into())),
                    ("tags".into(), strs(&["a", "b"])),
                ],
            )
            .unwrap();
            // industry matches (fires via FieldEqual); tags share nothing
            // (Overlap already false) — only one disjunct is ever true here.
            db.insert_node(
                "Person",
                "p1",
                vec![
                    ("industry".into(), Value::Str("tech".into())),
                    ("tags".into(), strs(&["x", "y"])),
                ],
            )
            .unwrap();
            // industry differs (FieldEqual false); tags fully overlap
            // (Overlap true) — fires via the other disjunct.
            db.insert_node(
                "Person",
                "p2",
                vec![
                    ("industry".into(), Value::Str("law".into())),
                    ("tags".into(), strs(&["a", "b"])),
                ],
            )
            .unwrap();
        },
        &[("p1", "o1"), ("p2", "o1")],
        |db| {
            // p1's Overlap branch was already false (tags share nothing with
            // o1). Changing industry now breaks the FieldEqual branch too,
            // so BOTH disjuncts are false and the edge must retract.
            db.set_prop("p1", "industry", Value::Str("law".into()))
                .unwrap();
        },
        &[("p2", "o1")],
    );
}

// ---------------------------------------------------------------------------
// 9. via-hop (via_label + via_edge) — delete one via edge. The 0.5.x shape.
// ---------------------------------------------------------------------------
#[test]
fn via_hop_round_trip() {
    round_trip(
        "via-hop",
        "MATCHED",
        "via_case",
        vec![RuleDef {
            name: "via_case".into(),
            src_label: "Person".into(),
            dst_label: "Project".into(),
            predicate: Predicate::FieldEqual {
                field: "tag".into(),
            },
            edge_type: "MATCHED".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: Some("Org".into()),
            via_edge: Some("WORKS_AT".into()),
            via_dir: None,
        }],
        |db| {
            // p1 reaches proj1 through TWO via-orgs (org1a, org1b); p2 reaches
            // it through exactly one (org2). Deleting only one of p1's via
            // edges must not retract p1's MATCHED edge — org1b still supports
            // it — while p2 loses its only via edge and does retract. A rule
            // that (re-)read topology from the post-snapshot write overlay
            // alone (the 0.5.x bug) would not see org1b, which lives only in
            // the pre-snapshot base topology, and would wrongly retract p1's
            // edge too.
            db.insert_node(
                "Org",
                "org1a",
                vec![("tag".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node(
                "Org",
                "org1b",
                vec![("tag".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node(
                "Org",
                "org2",
                vec![("tag".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node(
                "Project",
                "proj1",
                vec![("tag".into(), Value::Str("tech".into()))],
            )
            .unwrap();
            db.insert_node("Person", "p1", vec![]).unwrap();
            db.insert_node("Person", "p2", vec![]).unwrap();
            db.insert_edge("WORKS_AT", "p1", "org1a").unwrap();
            db.insert_edge("WORKS_AT", "p1", "org1b").unwrap();
            db.insert_edge("WORKS_AT", "p2", "org2").unwrap();
        },
        &[("p1", "proj1"), ("p2", "proj1")],
        |db| {
            // Delete one of p1's two via edges (p1 keeps org1b, so its
            // MATCHED edge must survive) and p2's only via edge (p2's MATCHED
            // edge must retract).
            db.delete_edge("WORKS_AT", "p1", "org1a").unwrap();
            db.delete_edge("WORKS_AT", "p2", "org2").unwrap();
        },
        &[("p1", "proj1")],
    );
}

// ---------------------------------------------------------------------------
// 10. Two-rule chain (rule A's edge_type is rule B's via_edge) — break rule
//     A's predicate for one source, assert rule B's edge went with it.
// ---------------------------------------------------------------------------
#[test]
fn chain_round_trip() {
    round_trip(
        "chain",
        "RELATED",
        "related",
        vec![
            // Rule A: Task -> Team, KeyMatch(team_id).
            RuleDef {
                name: "assigned".into(),
                src_label: "Task".into(),
                dst_label: "Team".into(),
                predicate: Predicate::KeyMatch {
                    field: "team_id".into(),
                },
                edge_type: "ASSIGNED_TO".into(),
                weight_prop: None,
                max_edges: None,
                approximate: false,
                via_label: None,
                via_edge: None,
                via_dir: None,
            },
            // Rule B: Team -> Task, via Tasks ASSIGNED_TO this Team,
            // FieldEqual(tag) between via-Task and dst-Task.
            RuleDef {
                name: "related".into(),
                src_label: "Team".into(),
                dst_label: "Task".into(),
                predicate: Predicate::FieldEqual {
                    field: "tag".into(),
                },
                edge_type: "RELATED".into(),
                weight_prop: None,
                max_edges: None,
                approximate: false,
                via_label: Some("Task".into()),
                via_edge: Some("ASSIGNED_TO".into()),
                via_dir: Some(Direction::In),
            },
        ],
        |db| {
            db.insert_node("Team", "teamX", vec![]).unwrap();
            db.insert_node(
                "Task",
                "t1",
                vec![
                    ("team_id".into(), Value::Str("teamX".into())),
                    ("tag".into(), Value::Str("backend".into())),
                ],
            )
            .unwrap();
            db.insert_node(
                "Task",
                "t2",
                vec![
                    ("team_id".into(), Value::Str("teamX".into())),
                    ("tag".into(), Value::Str("frontend".into())),
                ],
            )
            .unwrap();
            // Not assigned to teamX (no Team "other" node exists).
            db.insert_node(
                "Task",
                "t3",
                vec![
                    ("team_id".into(), Value::Str("other".into())),
                    ("tag".into(), Value::Str("backend".into())),
                ],
            )
            .unwrap();
        },
        // ASSIGNED_TO: t1->teamX, t2->teamX (t3 not assigned).
        // Via set for teamX (Tasks ASSIGNED_TO teamX) = {t1, t2}.
        // RELATED fires teamX->dst whenever some via Task shares `tag` with dst:
        //   dst t1(backend): via t1(backend) matches.
        //   dst t2(frontend): via t2(frontend) matches (self).
        //   dst t3(backend): via t1(backend) matches.
        &[("teamX", "t1"), ("teamX", "t2"), ("teamX", "t3")],
        |db| {
            // Break rule A's predicate for t1: clear its key field so
            // ASSIGNED_TO(t1->teamX) retracts. The via set shrinks to {t2}
            // (frontend only), so rule B's RELATED edges recompute:
            //   dst t1(backend): via t2(frontend) no longer matches -> retract.
            //   dst t2(frontend): via t2(frontend, self) still matches -> kept.
            //   dst t3(backend): via t2(frontend) no longer matches -> retract.
            db.set_prop("t1", "team_id", Value::Str("".into())).unwrap();
        },
        &[("teamX", "t2")],
    );
}
