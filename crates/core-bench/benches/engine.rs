//! Criterion harness for the Plan 8 hot paths.
//!
//! Bench IDs are binding for later tasks — do not rename:
//! `ingest_10k_nodes`, `neighborhood_depth1`, `neighborhood_depth2`,
//! `cypher_scan_filter_project`, `cypher_two_hop_join`,
//! `rule_incremental_fire`, `rule_backfill_10k`, `explain_pair`,
//! `explain_pair_dense`, `vector_rule_update`, `read_contention_1r0w`,
//! `read_contention_4r1w`, `read_contention_16r1w`,
//! `backfill_field_equal_5k`.

use core_api::{AutoFk, Dir, GraphDb, IngestOptions, Predicate, RuleDef, SharedDb, Value};
use core_storage::RealFs;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

const N: usize = 10_000;
const SEED: u64 = 0xA5A5_5A5A_C0DE_4B1D;
/// Mixed dims so `vector_rule_update`'s ScanAll dim-reject is live.
/// `idx % 3` → 32 / 64 / 128; flip embeddings keep the same `idx` (same dim).
const EMBED_DIMS: [usize; 3] = [32, 64, 128];
/// People `2..=HUB_PEOPLE+1` KeyMatch to `org-0001`, so that org is the dst
/// of ≥1k `WORKS_AT` provenance triples. `person-0001` stays off the hub
/// so the existing neighborhood benches keep a small depth-2 frontier.
const HUB_PEOPLE: usize = 1200;
const HUB_ORG: usize = 1;
const SCAN_FILTER: &str = "MATCH (n:Person) WHERE n.age > 40 RETURN n LIMIT 100";
const TWO_HOP: &str =
    "MATCH (p:Person)-[:ON_PROJECT]->(proj:Project)<-[:ON_PROJECT]-(q:Person) RETURN p, proj, q LIMIT 100";

fn counts(nodes: usize) -> (usize, usize, usize) {
    let n_orgs = nodes / 6;
    let n_projects = nodes / 3;
    let n_people = nodes - n_orgs - n_projects;
    (n_orgs, n_projects, n_people)
}

fn person_org(i: usize, n_orgs: usize) -> usize {
    if (2..=HUB_PEOPLE + 1).contains(&i) {
        HUB_ORG
    } else if i == 1 {
        2.min(n_orgs.max(1))
    } else {
        (i - 1) % n_orgs.max(1) + 1
    }
}

fn mix(seed: u64, a: u64, b: u64) -> u64 {
    let mut z = seed
        .wrapping_add(a.wrapping_mul(0x9E3779B97F4A7C15))
        .wrapping_add(b.wrapping_mul(0xBF58476D1CE4E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn skill_list(home: usize, n_skills: usize) -> Value {
    let n = n_skills.max(1);
    Value::List(
        (0..3)
            .map(|k| {
                let i = (home + k - 1) % n + 1;
                Value::Str(format!("s{i:04}"))
            })
            .collect(),
    )
}

fn coords(i: usize) -> Value {
    let lat = 25.0 + ((i * 17) % 500) as f64 * 0.08;
    let lon = -120.0 + ((i * 13) % 700) as f64 * 0.1;
    Value::List(vec![Value::Float(lat), Value::Float(lon)])
}

fn embedding(seed: u64, idx: u64) -> Value {
    let dim = EMBED_DIMS[(idx % 3) as usize];
    Value::List(
        (0..dim)
            .map(|d| {
                let bits = mix(seed, idx.wrapping_add(1), d as u64);
                let mut f = (bits as f64) / (u64::MAX as f64) * 2.0 - 1.0;
                if f == 0.0 {
                    f = 1.0;
                }
                Value::Float(f)
            })
            .collect(),
    )
}

type PropRow = BTreeMap<String, Value>;

fn row(pairs: Vec<(&str, Value)>) -> PropRow {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Seeded people / orgs / projects: skills windows, numeric years, [lat,lon],
/// dim-64 embeddings. Total node count is `nodes`.
fn dataset_rows(nodes: usize, seed: u64) -> (Vec<PropRow>, Vec<PropRow>, Vec<PropRow>) {
    let (n_orgs, n_projects, n_people) = counts(nodes);
    let orgs = (1..=n_orgs)
        .map(|i| {
            row(vec![
                ("id", Value::Str(format!("org-{i:04}"))),
                ("year", Value::Int(1980 + (i as i64 % 45))),
                ("loc", coords(i)),
                ("emb", embedding(seed, i as u64)),
                ("skills", skill_list(i, n_projects)),
            ])
        })
        .collect();
    let projects = (1..=n_projects)
        .map(|i| {
            let org = (i - 1) % n_orgs.max(1) + 1;
            row(vec![
                ("id", Value::Str(format!("proj-{i:04}"))),
                ("org_id", Value::Str(format!("org-{org:04}"))),
                ("year", Value::Int(1980 + (i as i64 % 45))),
                ("loc", coords(i + n_orgs)),
                ("emb", embedding(seed, (i + n_orgs) as u64)),
                ("skills", skill_list(i, n_projects)),
            ])
        })
        .collect();
    let people = (1..=n_people)
        .map(|i| {
            let org = person_org(i, n_orgs);
            let proj = (i - 1) % n_projects.max(1) + 1;
            row(vec![
                ("id", Value::Str(format!("person-{i:04}"))),
                ("org_id", Value::Str(format!("org-{org:04}"))),
                ("project_id", Value::Str(format!("proj-{proj:04}"))),
                ("age", Value::Int(18 + (i as i64 % 53))),
                ("year", Value::Int(1980 + (i as i64 % 45))),
                ("loc", coords(i + n_orgs + n_projects)),
                ("emb", embedding(seed, (i + n_orgs + n_projects) as u64)),
                ("skills", skill_list(proj, n_projects)),
            ])
        })
        .collect();
    (orgs, projects, people)
}

fn ingest_opts() -> IngestOptions {
    IngestOptions {
        key_field: "id".into(),
        auto_fk: AutoFk::Off,
    }
}

fn populate(db: &mut GraphDb<RealFs>, nodes: usize, seed: u64) {
    let (orgs, projects, people) = dataset_rows(nodes, seed);
    let opts = ingest_opts();
    db.ingest("Org", orgs, &opts).expect("ingest orgs");
    db.ingest("Project", projects, &opts)
        .expect("ingest projects");
    db.ingest("Person", people, &opts).expect("ingest people");
}

fn tmp_dir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-bench-{}-{}-{}",
        std::process::id(),
        n,
        nanos
    ))
}

/// Seeded 10k-shaped graph (no rules). Binding signature for later tasks.
fn bench_db(nodes: usize, seed: u64) -> GraphDb<RealFs> {
    let mut db = GraphDb::open(&tmp_dir()).expect("open bench db");
    populate(&mut db, nodes, seed);
    db
}

fn rule_works_at() -> RuleDef {
    RuleDef {
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
    }
}

fn rule_on_project() -> RuleDef {
    RuleDef {
        name: "on_project".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::KeyMatch {
            field: "project_id".into(),
        },
        edge_type: "ON_PROJECT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn rule_skill_fit() -> RuleDef {
    RuleDef {
        name: "skill_fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::Overlap {
            field: "skills".into(),
            min: 0.5,
        },
        edge_type: "FIT".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn rule_vector_sim() -> RuleDef {
    RuleDef {
        name: "similar_emb".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.95,
        },
        edge_type: "SIMILAR".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// Three rules including overlap — the `rule_incremental_fire` shape.
fn install_three_rules(db: &mut GraphDb<RealFs>) {
    db.create_rule(rule_works_at()).expect("works_at");
    db.create_rule(rule_on_project()).expect("on_project");
    db.create_rule(rule_skill_fit()).expect("skill_fit");
}

fn bench_db_ruled(nodes: usize, seed: u64) -> GraphDb<RealFs> {
    let mut db = bench_db(nodes, seed);
    install_three_rules(&mut db);
    db
}

fn empty_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn rule_field_equal_scale() -> RuleDef {
    RuleDef {
        name: "shared_group".into(),
        src_label: "Src".into(),
        dst_label: "Dst".into(),
        predicate: Predicate::FieldEqual {
            field: "group".into(),
        },
        edge_type: "GROUPED".into(),
        weight_prop: None,
        // Hard cap per source — exercises the streaming per-source budget path,
        // not the cross-product materialisation path.
        max_edges: Some(5),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// 5k × 5k FieldEqual backfill scale probe.
///
/// Ingests 5 000 "Src" nodes and 5 000 "Dst" nodes that all share the same
/// `group = "shared"` value, creating a 25 M-pair cross-product scenario.
/// `max_edges = Some(5)` caps each source at 5 edges, so the streaming
/// `apply_streaming_create_top_k` path terminates after emitting at most
/// 5 edges per source rather than materialising all 25 M pairs.  The bench
/// pins the wall time of that streaming path so CI catches future regressions
/// that would reintroduce cross-product materialisation.
fn field_equal_scale_backfill(c: &mut Criterion) {
    const SCALE: usize = 5_000;
    let opts = ingest_opts();

    let src_rows: Vec<PropRow> = (0..SCALE)
        .map(|i| {
            row(vec![
                ("id", Value::Str(format!("src-{i:05}"))),
                ("group", Value::Str("shared".into())),
            ])
        })
        .collect();
    let dst_rows: Vec<PropRow> = (0..SCALE)
        .map(|i| {
            row(vec![
                ("id", Value::Str(format!("dst-{i:05}"))),
                ("group", Value::Str("shared".into())),
            ])
        })
        .collect();

    c.bench_function("backfill_field_equal_5k", |b| {
        b.iter_batched(
            || {
                let mut db = GraphDb::open(&tmp_dir()).expect("open");
                db.ingest("Src", src_rows.clone(), &opts)
                    .expect("ingest src");
                db.ingest("Dst", dst_rows.clone(), &opts)
                    .expect("ingest dst");
                db
            },
            |mut db| {
                db.create_rule(rule_field_equal_scale())
                    .expect("backfill field_equal");
                black_box(db.edge_count());
            },
            BatchSize::PerIteration,
        );
    });
}

fn engine_benches(c: &mut Criterion) {
    ingest_10k_nodes(c);
    neighborhood(c);
    cypher(c);
    rules(c);
    explain_pair(c);
    vector_rule_update(c);
    vector_semantic_backfill(c);
    field_equal_scale_backfill(c);
    open_store(c);
    contention(c);
}

// ── Store open ───────────────────────────────────────────────────────────────
//
// Hook bodies (`touch`, `recall`) open the store on every prompt and every
// edit, so open time is latency the user feels rather than a startup cost
// amortised over a long-lived process. Both benches build the same store; they
// differ only in whether a snapshot stands in for the log.

/// A store shaped like a code graph: ~20k nodes, ~13k edges, 400 nodes carrying
/// a 4 KB body, and five rules — two of them over the list-valued property that
/// the co-change and authorship rules key on.
fn open_shaped_store(dir: &std::path::Path) {
    let mut db = GraphDb::open(dir).expect("open");
    db.create_rule(rule_skill_fit()).expect("skill_fit");
    db.create_rule(rule_works_at()).expect("works_at");
    db.create_rule(rule_on_project()).expect("on_project");
    db.create_rule(RuleDef {
        name: "co_changed".into(),
        src_label: "File".into(),
        dst_label: "File".into(),
        predicate: Predicate::Overlap {
            field: "commits".into(),
            min: 0.5,
        },
        edge_type: "CO_CHANGED".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .expect("co_changed");
    db.create_rule(RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: Predicate::Overlap {
            field: "commits".into(),
            min: 0.5,
        },
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(20),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(core_storage::Direction::In),
    })
    .expect("knows");

    populate(&mut db, N, SEED);

    // 400 files with bodies and commit lists, plus the authors they hang off.
    // Forty authors rather than a handful: the via-hop rule expands every one of
    // a source's files against every destination, so a store where four people
    // own a hundred files each takes minutes to replay and is no use as a bench.
    let body = "x".repeat(4096);
    for a in 0..40u32 {
        db.insert_node("Author", &format!("author-{a}"), vec![])
            .expect("author");
    }
    for f in 0..400u32 {
        let commits: Vec<Value> = (0..24)
            .map(|j| Value::Str(format!("sha-{}-{}", f % 40, j)))
            .collect();
        db.insert_node(
            "File",
            &format!("file-{f}"),
            vec![
                ("body".into(), Value::Str(body.clone())),
                ("commits".into(), Value::List(commits)),
                (
                    "top_author_id".into(),
                    Value::Str(format!("author-{}", f % 40)),
                ),
            ],
        )
        .expect("file");
        db.insert_edge(
            "TOP_AUTHOR",
            &format!("file-{f}"),
            &format!("author-{}", f % 40),
        )
        .expect("top_author");
    }
}

fn open_store(c: &mut Criterion) {
    let wal_dir = tmp_dir();
    open_shaped_store(&wal_dir);

    // The snapshot store gets the same log, then replaces it with a snapshot
    // and one trailing record — the state a store is in after `snapshot`.
    let snap_dir = tmp_dir();
    std::fs::create_dir_all(&snap_dir).expect("mkdir");
    std::fs::copy(wal_dir.join("wal.bin"), snap_dir.join("wal.bin")).expect("copy wal");
    {
        let mut db = GraphDb::open(&snap_dir).expect("open snap");
        db.snapshot().expect("snapshot");
        db.set_prop("file-0", "touched", Value::Int(1))
            .expect("tail");
    }

    c.bench_function("open_wal_only_this_repo_shape", |b| {
        b.iter(|| {
            let db = GraphDb::open(&wal_dir).expect("open");
            black_box(db.node_count())
        });
    });
    c.bench_function("open_with_snapshot_this_repo_shape", |b| {
        b.iter(|| {
            let db = GraphDb::open(&snap_dir).expect("open");
            black_box(db.node_count())
        });
    });
}

fn ingest_10k_nodes(c: &mut Criterion) {
    let (orgs, projects, people) = dataset_rows(N, SEED);
    c.bench_function("ingest_10k_nodes", |b| {
        b.iter_batched(
            || {
                let db = GraphDb::open(&tmp_dir()).expect("open");
                (db, orgs.clone(), projects.clone(), people.clone())
            },
            |(mut db, orgs, projects, people)| {
                let opts = ingest_opts();
                db.ingest("Org", orgs, &opts).expect("orgs");
                db.ingest("Project", projects, &opts).expect("projects");
                db.ingest("Person", people, &opts).expect("people");
                black_box(db.node_count());
            },
            BatchSize::PerIteration,
        );
    });
}

fn neighborhood(c: &mut Criterion) {
    let db = bench_db_ruled(N, SEED);
    let start = db.node_ref("person-0001").expect("person-0001");
    c.bench_function("neighborhood_depth1", |b| {
        b.iter(|| {
            black_box(start.neighborhood(black_box(1), None, Dir::Both));
        });
    });
    c.bench_function("neighborhood_depth2", |b| {
        b.iter(|| {
            black_box(start.neighborhood(black_box(2), None, Dir::Both));
        });
    });
}

fn cypher(c: &mut Criterion) {
    let db = bench_db_ruled(N, SEED);
    let params = empty_params();
    c.bench_function("cypher_scan_filter_project", |b| {
        b.iter(|| {
            black_box(
                db.query(black_box(SCAN_FILTER), &params)
                    .expect("scan filter"),
            );
        });
    });
    c.bench_function("cypher_two_hop_join", |b| {
        b.iter(|| {
            black_box(db.query(black_box(TWO_HOP), &params).expect("two hop"));
        });
    });
}

fn rules(c: &mut Criterion) {
    let mut db = bench_db_ruled(N, SEED);
    let (_n_orgs, n_projects, _n_people) = counts(N);
    let mut flip = 0u64;
    c.bench_function("rule_incremental_fire", |b| {
        b.iter(|| {
            flip += 1;
            let home = if flip.is_multiple_of(2) { 1 } else { 2 };
            db.set_prop("person-0001", "skills", skill_list(home, n_projects))
                .expect("skills update");
        });
    });

    c.bench_function("rule_backfill_10k", |b| {
        b.iter_batched(
            || bench_db(N, SEED),
            |mut db| {
                db.create_rule(rule_skill_fit()).expect("backfill overlap");
                black_box(db.edge_count());
            },
            BatchSize::PerIteration,
        );
    });
}

/// Bench-only 20k-triple hub: one Org + 20k People all KeyMatch to it.
/// The 10k `bench_db` hub (1.2k triples) made the full provenance walk
/// indistinguishable from the sparse pair (~45 µs); this variant exists
/// so O(total-provenance) vs O(degree) is measurable.
const HUB_DENSE: usize = 20_000;

fn bench_db_hub_dense() -> GraphDb<RealFs> {
    let mut db = GraphDb::open(&tmp_dir()).expect("open dense hub");
    let opts = ingest_opts();
    db.ingest(
        "Org",
        vec![row(vec![("id", Value::Str("org-0001".into()))])],
        &opts,
    )
    .expect("org");
    let people: Vec<_> = (1..=HUB_DENSE)
        .map(|i| {
            row(vec![
                ("id", Value::Str(format!("person-{i:05}"))),
                ("org_id", Value::Str("org-0001".into())),
            ])
        })
        .collect();
    db.ingest("Person", people, &opts).expect("people");
    db.create_rule(rule_works_at()).expect("works_at");
    db
}

fn explain_pair(c: &mut Criterion) {
    let db = bench_db_ruled(N, SEED);
    c.bench_function("explain_pair", |b| {
        b.iter(|| {
            black_box(
                db.explain(black_box("person-0001"), black_box("proj-0001"))
                    .expect("explain"),
            );
        });
    });
    let dense = bench_db_hub_dense();
    c.bench_function("explain_pair_dense", |b| {
        b.iter(|| {
            black_box(
                dense
                    .explain(black_box("org-0001"), black_box("person-00002"))
                    .expect("explain dense"),
            );
        });
    });
}

fn vector_rule_update(c: &mut Criterion) {
    let mut db = bench_db(N, SEED);
    db.create_rule(rule_vector_sim()).expect("vector rule");
    let idx = (1 + n_orgs_projects()) as u64;
    let a = embedding(SEED, idx);
    let b = embedding(SEED ^ 0xDEAD_BEEF, idx);
    let mut flip = false;
    c.bench_function("vector_rule_update", |bch| {
        bch.iter(|| {
            flip = !flip;
            let emb = if flip { a.clone() } else { b.clone() };
            db.set_prop("person-0001", "emb", emb)
                .expect("embedding update");
        });
    });
}

fn n_orgs_projects() -> usize {
    let (n_orgs, n_projects, _) = counts(N);
    n_orgs + n_projects
}

/// 5k-scale semantic-backfill probe.
///
/// Creates 500 "Doc" nodes each with a dim=1536 random embedding and
/// immediately backfills a VectorSimilar rule at min=0.85.  At min=0.85 with
/// uniformly random high-dimensional vectors the acceptance rate is near zero,
/// so the early-exit fires after the first checkpoint (~192 elements) for
/// almost every pair — the core win this bench is designed to capture.
///
/// 500 nodes is 1/10 of the 5k dogfood target; timing × 100 approximates the
/// full 5k scenario (both backfill time and pairs count scale quadratically
/// with n, so the actual 5k time scales by n²/n² = 1 — the multiplier is 10²
/// = 100 for the pairs evaluated, but the linear overhead per-node also
/// contributes).  Honest before/after numbers are captured here.
///
/// Binding name: `vector_semantic_backfill_500` — used in task-3 report.
fn vector_semantic_backfill(c: &mut Criterion) {
    const SEM_N: usize = 500;
    const SEM_DIM: usize = 1536;

    let rows: Vec<_> = (0..SEM_N)
        .map(|i| {
            // Produce a dim=1536 random embedding from the bench seed.
            let emb_val = Value::List(
                (0..SEM_DIM)
                    .map(|d| {
                        let bits = mix(SEED, i as u64 + 1, d as u64);
                        let mut f = (bits as f64) / (u64::MAX as f64) * 2.0 - 1.0;
                        if f == 0.0 {
                            f = 1.0;
                        }
                        Value::Float(f)
                    })
                    .collect(),
            );
            row(vec![
                ("id", Value::Str(format!("doc-{i:04}"))),
                ("emb", emb_val),
            ])
        })
        .collect();

    let rule = RuleDef {
        name: "sem_sim".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.85,
        },
        edge_type: "SEM_SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };

    c.bench_function("vector_semantic_backfill_500", |b| {
        b.iter_batched(
            || {
                let mut db = GraphDb::open(&tmp_dir()).expect("open");
                db.ingest("Doc", rows.clone(), &ingest_opts())
                    .expect("ingest docs");
                db
            },
            |mut db| {
                db.create_rule(rule.clone()).expect("create sem_sim");
                black_box(db.edge_count());
            },
            BatchSize::PerIteration,
        );
    });

    // Approximate variant: same 500-node 1536-D setup with approximate=true
    // (IVF-Flat candidate path). Target: faster than exact; recall ≥ 0.90.
    // Binding name: `vector_semantic_backfill_500_approximate` — task-4 report.
    let approx_rule = RuleDef {
        name: "sem_sim_approx".into(),
        src_label: "Doc".into(),
        dst_label: "Doc".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 0.85,
        },
        edge_type: "SEM_SIM_APPROX".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };

    c.bench_function("vector_semantic_backfill_500_approximate", |b| {
        b.iter_batched(
            || {
                let mut db = GraphDb::open(&tmp_dir()).expect("open");
                db.ingest("Doc", rows.clone(), &ingest_opts())
                    .expect("ingest docs");
                db
            },
            |mut db| {
                db.create_rule(approx_rule.clone())
                    .expect("create sem_sim_approx");
                black_box(db.edge_count());
            },
            BatchSize::PerIteration,
        );
    });
}

fn shared_ruled() -> SharedDb {
    let db = SharedDb::open(&tmp_dir()).expect("shared open");
    {
        let mut w = db.write();
        populate(&mut w, N, SEED);
        install_three_rules(&mut w);
    }
    db
}

fn run_contention(db: &SharedDb, n_readers: usize, reads: usize, writes: usize) {
    let start = Arc::new(Barrier::new(n_readers + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..n_readers)
        .map(|i| {
            let db = db.clone();
            let start = Arc::clone(&start);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                start.wait();
                let key = format!("person-{:04}", (i % 64) + 1);
                for _ in 0..reads {
                    let g = db.read();
                    let n = g.node_ref(&key).expect("reader key");
                    black_box(n.neighborhood(1, None, Dir::Both));
                }
                // Keep reading until the writer finishes so the lock stays
                // contended for the whole write burst.
                while !stop.load(Ordering::Acquire) {
                    let g = db.read();
                    let n = g.node_ref(&key).expect("reader key");
                    black_box(n.neighborhood(1, None, Dir::Both));
                }
            })
        })
        .collect();

    start.wait();
    for k in 0..writes {
        db.write()
            .set_prop("person-0001", "age", Value::Int(40 + k as i64))
            .expect("writer prop");
    }
    stop.store(true, Ordering::Release);
    for h in handles {
        h.join().expect("reader");
    }
}

fn contention(c: &mut Criterion) {
    let db = shared_ruled();
    c.bench_function("read_contention_1r0w", |b| {
        b.iter(|| run_contention(&db, 1, 16, 0));
    });
    c.bench_function("read_contention_4r1w", |b| {
        b.iter(|| run_contention(&db, 4, 16, 16));
    });
    c.bench_function("read_contention_16r1w", |b| {
        b.iter(|| run_contention(&db, 16, 16, 16));
    });
}

fn bench_config() -> Criterion {
    Criterion::default()
        .sample_size(12)
        .warm_up_time(Duration::from_millis(400))
        .measurement_time(Duration::from_secs(2))
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = engine_benches
}
criterion_main!(benches);
