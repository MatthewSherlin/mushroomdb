//! Scoped reads — the RBAC read contract, in the engine (v0.6.10 §5.3, §5.4).
//!
//! A scoped read checks the **subject** first: a key outside the scope is
//! indistinguishable from a key that does not exist. Only then is every other
//! node the answer would mention filtered to the scope.
//!
//! This is the contract `crates/server/src/http.rs` wrote by hand on its
//! role-token branches. It lives here now, so every caller gets it — not only
//! the two HTTP routes that happened to implement it.
//!
//! The `*_masked` methods deliberately do **not** make the subject check: they
//! serve full-token client masks, where the caller already knows the key
//! exists. That difference is the whole reason these methods exist.

use mushroomdb::{
    with_pairwise_caps, AlgoDir, Dir, Explanation, GraphDb, GraphError, NodeMask, Predicate,
    RealFs, RuleDef, Value, NS_PROP,
};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-scoped-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `a → b → c`, the chain every test here reasons about.
fn chain(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for key in ["a", "b", "c"] {
        db.insert_node("Doc", key, vec![]).unwrap();
    }
    db.insert_edge("LINKS", "a", "b").unwrap();
    db.insert_edge("LINKS", "b", "c").unwrap();
    (dir, db)
}

/// `a → b` and `a → c`: one subject, two neighbours, so a count can tell the
/// difference between filtering and not.
fn star(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for key in ["a", "b", "c"] {
        db.insert_node("Doc", key, vec![]).unwrap();
    }
    db.insert_edge("LINKS", "a", "b").unwrap();
    db.insert_edge("LINKS", "a", "c").unwrap();
    (dir, db)
}

/// `alice -[WORKS_AT]→ techcorp`, and a via-hop rule that derives
/// `alice -[FIT]→ proj_a` **because of `techcorp`**.
///
/// The via node is the explanation's evidence and the only node in it that the
/// two subjects do not already name — which is exactly why an `Explanation`
/// cannot be redacted: it carries `via_edge`, the hop's *type*, and never the
/// hop's key.
fn via(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let tech = || vec![("industry".to_string(), Value::Str("tech".into()))];
    db.insert_node("Org", "techcorp", tech()).unwrap();
    db.insert_node("Person", "alice", tech()).unwrap();
    db.insert_node("Project", "proj_a", tech()).unwrap();
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();
    db.create_rule(RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    (dir, db)
}

/// The `via` fixture with the two things it deliberately left out: a
/// `weight_prop`, and two via nodes that score **differently**.
///
/// `alice` works at both orgs. The rule scores `NumericWithin` between the via
/// and `proj_a`, whose `rank` is `0.0`, with a tolerance of `1.0` — so the score
/// is `1.0 - rank`, and the engine stores the **max over every via**:
///
/// | via | `rank` | score |
/// |---|---|---|
/// | `org_visible` | 0.5 | 0.5 |
/// | `org_hidden` | 0.25 | **0.75** |
///
/// The stored weight is therefore `0.75`, a number only the hidden org
/// produced. Both ranks are exact binary fractions, so the scores are exact and
/// the assertions need no epsilon.
///
/// The predicate is `All([KeyMatch, NumericWithin])` rather than the bare
/// `NumericWithin` so that it is **direction-sensitive**: `KeyMatch` reads
/// `ref` from the *first* view and compares it to the *second* view's key, so
/// evaluating `(dst, via)` instead of `(via, dst)` scores nothing at all. A
/// symmetric predicate cannot tell those two apart, and the recomputation has
/// to ask the question the engine asked. `KeyMatch` scores 1.0 and `All` takes
/// the min, so the scores in the table above are unchanged.
fn via_weighted(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let org = |v: f64| {
        vec![
            ("rank".to_string(), Value::Float(v)),
            ("ref".to_string(), Value::Str("proj_a".into())),
        ]
    };
    db.insert_node("Org", "org_visible", org(0.5)).unwrap();
    db.insert_node("Org", "org_hidden", org(0.25)).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node(
        "Project",
        "proj_a",
        vec![("rank".into(), Value::Float(0.0))],
    )
    .unwrap();
    db.insert_edge("WORKS_AT", "alice", "org_visible").unwrap();
    db.insert_edge("WORKS_AT", "alice", "org_hidden").unwrap();
    db.create_rule(RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::All(vec![
            Predicate::KeyMatch {
                field: "ref".into(),
            },
            Predicate::NumericWithin {
                field: "rank".into(),
                tolerance: 1.0,
            },
        ]),
        edge_type: "FIT".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    (dir, db)
}

/// The `via` fixture with a **namespace** on the via-hop rule, and two vias that
/// differ only in which namespace they sit in.
///
/// `org_hidden` is in `t1` and is the via the engine actually hopped through —
/// `fit` is scoped to `t1`, so `org_visible` in `t2` was never a candidate.
/// Both carry label `Org`, both satisfy the predicate, both are `WORKS_AT` from
/// `alice`. The only difference is the namespace.
///
/// The hop edges are **derived**, by a global `employs` rule, because a
/// user-written edge may not cross a namespace boundary
/// (`GraphError::CrossNamespace`). A global rule sees every node, so its edges
/// can — which is the chaining case [`Explanation::via_edge`] documents, and the
/// only way a visible out-of-namespace via can sit on `alice`'s hop edge at all.
fn via_namespaced(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let ns = |n: &str| (NS_PROP.to_string(), Value::Str(n.to_string()));
    let tech = |n: &str| vec![("industry".to_string(), Value::Str("tech".into())), ns(n)];
    db.insert_node("Org", "org_hidden", tech("t1")).unwrap();
    db.insert_node("Org", "org_visible", tech("t2")).unwrap();
    db.insert_node("Person", "alice", tech("t1")).unwrap();
    db.insert_node("Project", "proj_a", tech("t1")).unwrap();
    db.create_rule(RuleDef {
        name: "employs".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "WORKS_AT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    db.create_rule(RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
        namespace: Some("t1".into()),
    })
    .unwrap();
    (dir, db)
}

/// The `key` column of a neighborhood result, in row order.
fn keys_of(rs: &mushroomdb::ResultSet) -> Vec<String> {
    (0..rs.len())
        .map(|i| match rs.row(i)[0].as_ref() {
            Some(mushroomdb::Value::Str(s)) => s.clone(),
            other => panic!("the key column must be a string, got {other:?}"),
        })
        .collect()
}

/// A hidden subject answers exactly as an absent one does. Anything else is an
/// existence oracle: the caller learns `c` exists by being told it has edges.
#[test]
fn scoped_node_edges_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = chain("edges-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.node_edges_scoped("c", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        Ok(edges) => panic!("a hidden subject must not yield an edge list: {edges:?}"),
        Err(other) => panic!("expected KeyNotFound, got {other:?}"),
    }

    // The same answer an absent key gives — that is the point.
    match db.node_edges_scoped("never-existed", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// The subject is visible, so the call succeeds — but `b → c` names a node the
/// scope hides, and a visible subject must not become a window onto its hidden
/// neighbours.
#[test]
fn scoped_node_edges_drops_hidden_endpoints() {
    let (_dir, db) = chain("edges-hidden-endpoint");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    let edges = db.node_edges_scoped("b", &mask).unwrap();
    assert_eq!(edges.len(), 1, "only `a → b` survives the scope: {edges:?}");
    assert_eq!(edges[0].edge_type, "LINKS");
    assert_eq!(edges[0].src_key, "a");
    assert_eq!(edges[0].dst_key, "b");

    // Unscoped, `b` has both edges — the filter is doing the work, not the
    // fixture.
    assert_eq!(db.node_edges("b").unwrap().len(), 2);
}

/// `c` is *in* the scope, and still must not appear: the only path to it runs
/// through hidden `b`. A hidden node is not a stepping stone.
///
/// The start key is not a row of its own — that is `neighborhood_masked`'s
/// long-standing shape, which this wrapper must not change — so the correct
/// answer here is the empty set.
#[test]
fn scoped_neighborhood_never_crosses_a_hidden_node() {
    let (_dir, db) = chain("neighborhood-no-crossing");
    let mask = NodeMask::from_keys(&db, ["a", "c"]);

    let rs = db
        .neighborhood_scoped("a", 2, None, Dir::Both, &mask)
        .unwrap();
    assert_eq!(
        keys_of(&rs),
        Vec::<String>::new(),
        "`c` is reachable only through hidden `b`"
    );

    // Widen the scope to include `b` and `c` reappears — the fixture does reach
    // it, so the empty result above is the filter's doing.
    let all = NodeMask::from_keys(&db, ["a", "b", "c"]);
    let rs = db
        .neighborhood_scoped("a", 2, None, Dir::Both, &all)
        .unwrap();
    assert_eq!(keys_of(&rs), vec!["b".to_string(), "c".to_string()]);
}

/// The subject check applies to `neighborhood` on the same terms as to
/// `node_edges`: hidden and absent are one answer.
#[test]
fn scoped_neighborhood_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = chain("neighborhood-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.neighborhood_scoped("c", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.neighborhood_scoped("never-existed", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// The reader-snapshot twins carry the same contract. HTTP's role branches read
/// from a [`mushroomdb::ReaderSnapshot`], not a `GraphDb`, so a contract that
/// lived only on `GraphDb` would leave the RBAC path still hand-rolled.
#[test]
fn scoped_reads_on_a_reader_snapshot_honour_the_same_contract() {
    let (_dir, db) = chain("reader-snapshot");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);
    let snap = db.reader();

    match snap.node_edges_scoped("c", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }

    let edges = snap.node_edges_scoped("b", &mask).unwrap();
    assert_eq!(edges.len(), 1, "only `a → b` survives: {edges:?}");
    assert_eq!(edges[0].src_key, "a");
    assert_eq!(edges[0].dst_key, "b");

    let via_hidden = NodeMask::from_keys(&db, ["a", "c"]);
    let rs = snap
        .neighborhood_scoped("a", 2, None, Dir::Both, &via_hidden)
        .unwrap();
    assert_eq!(rs.len(), 0, "`c` is reachable only through hidden `b`");

    match snap.neighborhood_scoped("c", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

// ── degree, degrees, explain (§5.3, §5.4) ────────────────────────────────────

/// A count is a disclosure. Telling a caller that `a` has two neighbours when it
/// may see only one of them says a second node exists — the same leak
/// `scoped_node_edges_drops_hidden_endpoints` prevents, arrived at by
/// arithmetic instead of by name.
#[test]
fn scoped_degree_counts_only_visible_neighbours() {
    let (_dir, db) = star("degree-visible-only");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    assert_eq!(
        db.degree_scoped("a", None, AlgoDir::Out, &mask).unwrap(),
        1,
        "`c` is hidden, so it is not counted"
    );
    // Unscoped, `a` has two — the filter is doing the work, not the fixture.
    assert_eq!(db.degree("a", None, AlgoDir::Out).unwrap(), 2);

    // The edge type and direction legs stay honest under the filter.
    assert_eq!(
        db.degree_scoped("a", Some("LINKS"), AlgoDir::Both, &mask)
            .unwrap(),
        1
    );
    assert_eq!(
        db.degree_scoped("a", Some("NOSUCH"), AlgoDir::Both, &mask)
            .unwrap(),
        0,
        "an unknown edge type is still 0, not an error"
    );

    // Widen the scope and the hidden neighbour reappears.
    let all = NodeMask::from_keys(&db, ["a", "b", "c"]);
    assert_eq!(db.degree_scoped("a", None, AlgoDir::Out, &all).unwrap(), 2);
}

/// Hidden and absent are one answer here too: a degree of 0 for a hidden
/// subject would still confirm the key exists.
#[test]
fn scoped_degree_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = star("degree-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.degree_scoped("c", None, AlgoDir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.degree_scoped("never-existed", None, AlgoDir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// `degrees` takes a key list, so the scope has to bite twice: a hidden key
/// must not come back as a row, and the rows that do come back must carry
/// visible-only counts.
#[test]
fn scoped_degrees_omits_hidden_keys_from_input_and_output() {
    let (_dir, db) = star("degrees-omits-hidden");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);
    let keys: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();

    let got = db
        .degrees_scoped(Some(&keys), None, None, None, AlgoDir::Both, None, &mask)
        .unwrap();
    assert_eq!(
        got,
        vec![("a".to_string(), 1u64), ("b".to_string(), 1u64)],
        "`c` is hidden as a row, and as one of `a`'s neighbours"
    );

    // Unscoped: three rows, and `a` has two neighbours.
    let all = db
        .degrees(Some(&keys), None, None, None, AlgoDir::Both, None)
        .unwrap();
    assert_eq!(
        all,
        vec![
            ("a".to_string(), 2u64),
            ("b".to_string(), 1u64),
            ("c".to_string(), 1u64),
        ]
    );

    // The label-scan leg (`keys = None`) is filtered on the same terms.
    let scanned = db
        .degrees_scoped(None, Some("Doc"), None, None, AlgoDir::Both, None, &mask)
        .unwrap();
    assert_eq!(
        scanned.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"],
        "a label scan must not enumerate hidden nodes"
    );
}

/// A via-hop rule's evidence *is* the via node: `alice` and `proj_a` are linked
/// only because `techcorp` sits between them. `Explanation` names the hop's
/// edge type and never the hop's key, so there is nothing to redact — the
/// explanation goes, or the hidden node's existence is disclosed.
#[test]
fn scoped_explain_omits_a_path_through_a_hidden_node() {
    let (_dir, db) = via("explain-hidden-via");

    // Unscoped, the explanation exists — the fixture does derive the edge.
    let full = db.explain("alice", "proj_a").unwrap();
    assert_eq!(full.len(), 1, "fixture must derive one edge: {full:?}");
    assert_eq!(full[0].rule, "fit");
    assert_eq!(full[0].via_edge.as_deref(), Some("WORKS_AT"));

    // Both subjects visible, the via node hidden: nothing to say.
    let hides_via = NodeMask::from_keys(&db, ["alice", "proj_a"]);
    assert_eq!(
        db.explain_scoped("alice", "proj_a", &hides_via).unwrap(),
        Vec::<mushroomdb::Explanation>::new(),
        "the only evidence runs through hidden `techcorp`"
    );

    // Show the via node and the explanation returns intact, not redacted.
    let all = NodeMask::from_keys(&db, ["alice", "proj_a", "techcorp"]);
    assert_eq!(db.explain_scoped("alice", "proj_a", &all).unwrap(), full);

    // Either endpoint hidden — or absent — is `KeyNotFound` (§5.3).
    let hides_dst = NodeMask::from_keys(&db, ["alice", "techcorp"]);
    match db.explain_scoped("alice", "proj_a", &hides_dst) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "proj_a"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.explain_scoped("never-existed", "proj_a", &all) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// The weight a scoped explanation reports is the **visible corpus's** number.
///
/// A via-hop rule stores the max over every via node, so the stored weight can
/// be a score only a hidden via produced. Passing it through unchanged is the
/// same disclosure the dropped-explanation rule exists to prevent: a hidden node
/// influencing a number the caller reads (defect #3).
#[test]
fn scoped_explain_reports_the_weight_of_the_visible_vias_only() {
    let (_dir, db) = via_weighted("explain-weight-visible-only");

    // Unscoped, the stored weight is the hidden org's 0.75 — the max over both.
    let full = db.explain("alice", "proj_a").unwrap();
    assert_eq!(full.len(), 1, "fixture must derive one edge: {full:?}");
    assert_eq!(
        full[0].weight,
        Some(0.75),
        "fixture must store the max over both vias"
    );

    // Hide the org that set that max. The explanation survives — `org_visible`
    // vouches for it on its own — but the number must become `org_visible`'s.
    let hides_top = NodeMask::from_keys(&db, ["alice", "proj_a", "org_visible"]);
    let scoped = db.explain_scoped("alice", "proj_a", &hides_top).unwrap();
    assert_eq!(scoped.len(), 1, "a visible via still vouches: {scoped:?}");
    assert_eq!(
        scoped[0].weight,
        Some(0.5),
        "the weight must be the visible corpus's max, not the hidden via's 0.75"
    );
    // Nothing else about the explanation changes.
    assert_eq!(
        Explanation {
            weight: full[0].weight,
            ..scoped[0].clone()
        },
        full[0],
        "only the weight is rewritten"
    );

    // Show both vias and the stored number comes back.
    let all = NodeMask::from_keys(&db, ["alice", "proj_a", "org_visible", "org_hidden"]);
    assert_eq!(db.explain_scoped("alice", "proj_a", &all).unwrap(), full);

    // Hide every via and the explanation goes entirely, weight and all.
    let no_via = NodeMask::from_keys(&db, ["alice", "proj_a"]);
    assert_eq!(
        db.explain_scoped("alice", "proj_a", &no_via).unwrap(),
        Vec::<Explanation>::new()
    );
}

/// Only a via the **rule** could have hopped through may vouch for an
/// explanation.
///
/// The engine filters via candidates by the rule's namespace, so a visible via
/// in another namespace was never evidence for anything. Letting it vouch keeps
/// an explanation whose real evidence is a hidden node — the leak, dressed as a
/// visible witness (defect #4).
#[test]
fn scoped_explain_rejects_an_out_of_namespace_via_as_evidence() {
    let (_dir, db) = via_namespaced("explain-out-of-namespace-via");

    // The edge exists, and `org_hidden` (ns `t1`) is the only via that made it:
    // the rule is scoped to `t1`, so `org_visible` (ns `t2`) was never a hop.
    let full = db.explain("alice", "proj_a").unwrap();
    assert_eq!(full.len(), 1, "fixture must derive one edge: {full:?}");
    assert_eq!(full[0].via_edge.as_deref(), Some("WORKS_AT"));

    // `org_visible` is visible, carries `Org`, satisfies the predicate, and is
    // `WORKS_AT` from `alice` — everything but the namespace. It must not vouch.
    let hides_in_ns = NodeMask::from_keys(&db, ["alice", "proj_a", "org_visible"]);
    assert_eq!(
        db.explain_scoped("alice", "proj_a", &hides_in_ns).unwrap(),
        Vec::<Explanation>::new(),
        "an out-of-namespace via is not evidence the engine ever used"
    );

    // The in-namespace via, shown, does vouch.
    let shows_in_ns = NodeMask::from_keys(&db, ["alice", "proj_a", "org_hidden"]);
    assert_eq!(
        db.explain_scoped("alice", "proj_a", &shows_in_ns).unwrap(),
        full
    );
}

// ── pairwise_similar, search_hybrid (§5.3, §5.4) ─────────────────────────────

fn emb(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

/// Four keys with embeddings, one of which the scope hides — and the hidden one
/// is `a`'s nearest neighbour, so a `k = 1` answer has room for exactly one of
/// `hidden` and `b`.
fn pairwise_fixture(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for (key, v) in [
        ("a", [1.0, 0.0]),
        ("hidden", [1.0, 0.01]),
        ("b", [0.9, 0.436]),
        ("c", [0.0, 1.0]),
    ] {
        db.insert_node("Item", key, vec![("emb".into(), emb(&v))])
            .unwrap();
    }
    (dir, db)
}

/// A hidden vector packed into the matmul is a row every visible key is scored
/// against. It can take a visible neighbour's place in the top-`k`, and — because
/// the packed dimension is decided by a majority vote over the candidate rows —
/// it can decide whether a visible pair is scored **at all**. Filtering the
/// results afterwards leaves both effects standing.
///
/// The oracle is `pairwise_similar` over the visible keys alone: a scoped answer
/// that differs from it, in any row or any score, has let a hidden vector speak.
#[test]
fn scoped_pairwise_drops_hidden_keys_before_the_matmul() {
    let (_dir, db) = pairwise_fixture("pairwise-before-matmul");
    let all = ["a", "hidden", "b", "c"];
    let mask = NodeMask::from_keys(&db, ["a", "b", "c"]);

    // The fixture really does put `hidden` at the top of `a`'s list, so a
    // post-filter would have nothing left to report for `a`.
    let unscoped = db.pairwise_similar(&all, "emb", 1, 0.0).unwrap();
    let a_row = unscoped.iter().find(|(k, _)| k == "a").expect("`a` row");
    assert_eq!(
        a_row.1.first().map(|(k, _)| k.as_str()),
        Some("hidden"),
        "fixture must rank `hidden` first for `a`: {a_row:?}"
    );

    let got = db
        .pairwise_similar_scoped(&all, "emb", 1, 0.0, &mask)
        .unwrap();

    // Identical to the same call over the visible subset — the whole contract.
    let oracle = db
        .pairwise_similar(&["a", "b", "c"], "emb", 1, 0.0)
        .unwrap();
    assert_eq!(
        got, oracle,
        "a scope must read as a smaller key set, nothing more"
    );

    // Said again as names, so a regression reads as a leak and not as a diff.
    for (src, neigh) in &got {
        assert_ne!(src, "hidden", "a hidden key must not be a subject");
        for (dst, _) in neigh {
            assert_ne!(dst, "hidden", "a hidden key must not be a neighbour");
        }
    }
    let a_row = got.iter().find(|(k, _)| k == "a").expect("`a` row");
    assert_eq!(
        a_row.1.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["b"],
        "`b` takes the place `hidden` held; post-filtering would leave `a` empty"
    );
}

/// The dimension vote is the sharpest form of the same leak: `pairwise_similar`
/// packs at the modal dimension of its candidate rows and silently omits every
/// row of another width. Three hidden 2-d vectors outvote two visible 3-d ones,
/// so unscoped the two visible keys are not scored at all — a hidden vector
/// deciding a visible pair's score by deciding there isn't one.
#[test]
fn scoped_pairwise_hidden_vectors_do_not_carry_the_dimension_vote() {
    let dir = tmp("pairwise-dim-vote");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Item", "p", vec![("emb".into(), emb(&[1.0, 0.0, 0.0]))])
        .unwrap();
    db.insert_node("Item", "q", vec![("emb".into(), emb(&[0.8, 0.6, 0.0]))])
        .unwrap();
    for (key, v) in [("h1", [1.0, 0.0]), ("h2", [0.0, 1.0]), ("h3", [0.6, 0.8])] {
        db.insert_node("Item", key, vec![("emb".into(), emb(&v))])
            .unwrap();
    }
    let all = ["p", "q", "h1", "h2", "h3"];
    let mask = NodeMask::from_keys(&db, ["p", "q"]);

    // Unscoped, the 2-d majority wins the vote and the visible pair vanishes.
    let unscoped = db.pairwise_similar(&all, "emb", 5, 0.0).unwrap();
    let srcs: Vec<&str> = unscoped.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !srcs.contains(&"p") && !srcs.contains(&"q"),
        "fixture must let the hidden dimension win unscoped: {srcs:?}"
    );

    let got = db
        .pairwise_similar_scoped(&all, "emb", 5, 0.0, &mask)
        .unwrap();
    assert_eq!(
        got,
        db.pairwise_similar(&["p", "q"], "emb", 5, 0.0).unwrap(),
        "the scoped answer is the visible subset's answer"
    );
    let p_row = got.iter().find(|(k, _)| k == "p").expect("`p` row");
    assert_eq!(
        p_row.1.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["q"],
        "`p` and `q` are scored once the hidden rows cannot outvote them"
    );
}

/// The caps count the keys that actually reach the kernel, so the scope is
/// allowed to bring an over-cap call under the cap. That is a feature: the work
/// the cap exists to refuse is work this call no longer does.
#[test]
fn scoped_pairwise_caps_measure_the_filtered_set() {
    let dir = tmp("pairwise-caps");
    let mut db = GraphDb::open(&dir).unwrap();
    let keys = ["a", "b", "c", "d", "e"];
    for (i, key) in keys.iter().enumerate() {
        let t = i as f64 * 0.1;
        db.insert_node("Item", key, vec![("emb".into(), emb(&[1.0 - t, t]))])
            .unwrap();
    }
    let mask = NodeMask::from_keys(&db, ["a", "b", "c"]);

    // PAIRWISE_MAX_N = 3: five keys is over, the three visible ones are not.
    with_pairwise_caps(2, 3, || {
        match db.pairwise_similar(&keys, "emb", 5, 0.0) {
            Err(GraphError::QueryError { detail }) => {
                assert!(
                    detail.contains("PAIRWISE_MAX_N") && detail.contains("n=5"),
                    "unscoped must refuse the full key set: {detail}"
                );
            }
            other => panic!("expected the cap QueryError, got {other:?}"),
        }
        let got = db
            .pairwise_similar_scoped(&keys, "emb", 5, 0.0, &mask)
            .unwrap();
        assert_eq!(
            got.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"],
            "the post-filter count is under the cap, so the call succeeds"
        );
    });

    // And the cap still bites on the filtered count when *that* is over it —
    // the scope moves the measurement, it does not remove it.
    with_pairwise_caps(2, 2, || {
        match db.pairwise_similar_scoped(&keys, "emb", 5, 0.0, &mask) {
            Err(GraphError::QueryError { detail }) => assert!(
                detail.contains("n=3"),
                "the refusal must name the post-filter count, not 5: {detail}"
            ),
            other => panic!("expected the cap QueryError, got {other:?}"),
        }
    });
}

/// Eight hidden nodes outrank the two visible ones in **both** legs, and the
/// over-fetch is `4*k = 8`. Fuse first and the pool is spent entirely on hidden
/// hits, so filtering the fused list returns nothing; filter first and the `8`
/// is a budget of visible hits, so `k = 2` is honoured.
#[test]
fn scoped_hybrid_filters_before_fusion() {
    let dir = tmp("hybrid-before-fusion");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_fulltext("Item", "body").unwrap();
    // `h*` sorts before `v*`, and every node matches the text query and the
    // query vector identically, so the key tiebreak decides both rankings.
    let mut all: Vec<String> = (0..8).map(|i| format!("h{i}")).collect();
    all.extend((0..2).map(|i| format!("v{i}")));
    for key in &all {
        db.insert_node(
            "Item",
            key,
            vec![
                ("body".into(), Value::Str("unique".into())),
                ("emb".into(), emb(&[1.0, 0.0])),
            ],
        )
        .unwrap();
    }
    let mask = NodeMask::from_keys(&db, ["v0", "v1"]);
    let q = [1.0_f64, 0.0];

    // Unscoped, the visible pair is nowhere near the top — that is the fixture.
    let unscoped = db.search_hybrid("body", "unique", "emb", &q, Some("Item"), 2);
    assert_eq!(
        unscoped.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["h0", "h1"],
        "fixture must bury the visible nodes under the over-fetch"
    );

    let got = db.search_hybrid_scoped("body", "unique", "emb", &q, Some("Item"), 2, &mask);
    assert_eq!(
        got.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["v0", "v1"],
        "k=2 visible candidates exist, so k=2 must come back"
    );

    // Both legs really did contribute, and both ranked `v0` first: the ranks
    // that enter RRF are the visible corpus's ranks, not `v0`'s store-wide rank
    // of 9. One leg alone would score 1/61; two score 2/61, at the unchanged
    // constant 60.
    assert!(
        (got[0].1 - 2.0 / 61.0).abs() < 1e-12,
        "both legs must fuse `v0` at visible rank 1: {got:?}"
    );
    assert!(
        (got[1].1 - 2.0 / 62.0).abs() < 1e-12,
        "and `v1` at visible rank 2: {got:?}"
    );
}

/// The advice a warning gives has to be reachable from the call that triggered
/// it. `search_hybrid` and `search_hybrid_scoped` take neither `exact` nor
/// `where`, so the vector leg's line cannot tell a hybrid caller to "pass
/// exact=True" — they would go looking for an argument that does not exist.
///
/// The warning is not suppressed on this path: the approximation is just as
/// real, and a hybrid caller who read `mask=` as a promise of exhaustiveness is
/// exactly the reader it was written for. Only the advice changes, to name a
/// call that does accept the argument.
#[test]
fn the_hybrid_path_advises_a_call_the_hybrid_caller_can_make() {
    let dir = tmp("exactness-warning-hybrid");
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_fulltext("Item", "body").unwrap();
    for i in 0..8u32 {
        let x = 0.5 + i as f64 * 0.05;
        db.insert_node(
            "Item",
            &format!("v{i}"),
            vec![
                ("body".into(), Value::Str("unique".into())),
                ("emb".into(), emb(&[x, 1.0 - x])),
            ],
        )
        .unwrap();
    }
    db.create_rule(RuleDef {
        name: "ann".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 1.0,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    assert!(db.has_vector_rule("emb"), "an index must cover the field");

    let visible: Vec<String> = (0..6).map(|i| format!("v{i}")).collect();
    let mask = NodeMask::from_keys(&db, visible.iter().map(String::as_str));
    let q = [1.0_f64, 0.0];

    mushroomdb::ambiguous_exactness_warns_reset();
    db.search_hybrid_scoped("body", "unique", "emb", &q, Some("Item"), 3, &mask);
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        1,
        "a masked hybrid search rides the same approximate leg, and says so"
    );
    let hybrid = mushroomdb::ambiguous_exactness_last_warning()
        .expect("the warning that was counted must also have been recorded");
    assert!(
        !hybrid.contains("pass exact=True or a where= predicate"),
        "the hybrid line must not name arguments `search_hybrid` cannot accept: {hybrid}"
    );
    assert!(
        hybrid.contains("search_hybrid") && hybrid.contains("find_similar("),
        "it must say which call takes no exactness argument, and which one does: {hybrid}"
    );
    assert!(
        hybrid.contains("the vector leg of a masked hybrid search"),
        "and which of the caller's two legs it is about: {hybrid}"
    );

    // The direct call keeps the original advice: `exact` and `where` really are
    // its own parameters, so naming them is the whole point.
    mushroomdb::ambiguous_exactness_warns_reset();
    db.find_similar_vector_masked("emb", Some("Item"), &q, 3, 0.0, &mask);
    let direct =
        mushroomdb::ambiguous_exactness_last_warning().expect("the direct line is recorded");
    assert!(
        direct.contains("pass exact=True or a where= predicate"),
        "`find_similar` does take both, so its advice is unchanged: {direct}"
    );

    // Each caller shape gets its own once-per-index entry: a direct call that
    // warned first must not silence the hybrid line that carries the other
    // advice. Both have now warned, so both are silent from here.
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        1,
        "the direct line is not suppressed by the hybrid one"
    );
    db.search_hybrid_scoped("body", "unique", "emb", &q, Some("Item"), 3, &mask);
    db.find_similar_vector_masked("emb", Some("Item"), &q, 3, 0.0, &mask);
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        1,
        "and neither repeats on the same index"
    );
}

/// `mask=` alone rides the widening beam. That is deliberate — making a mask
/// imply `exact` would turn every existing masked caller's ANN into an O(n)
/// GEMM — but it cost a real integration team real time, because nothing said
/// so. The line is printed once per index, the way the dimension-mismatch skip
/// in `core_rules::hnsw` is.
#[test]
fn a_masked_search_without_exact_warns_once() {
    let dir = tmp("exactness-warning");
    let mut db = GraphDb::open(&dir).unwrap();
    for i in 0..8u32 {
        let x = 0.5 + i as f64 * 0.05;
        db.insert_node(
            "V",
            &format!("v{i}"),
            vec![("emb".into(), emb(&[x, 1.0 - x]))],
        )
        .unwrap();
    }
    db.create_rule(RuleDef {
        name: "ann".into(),
        src_label: "V".into(),
        dst_label: "V".into(),
        predicate: Predicate::VectorSimilar {
            field: "emb".into(),
            min: 1.0,
        },
        edge_type: "SIM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: true,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    assert!(db.has_vector_rule("emb"), "an index must cover the field");

    let visible: Vec<String> = (0..6).map(|i| format!("v{i}")).collect();
    let mask = NodeMask::from_keys(&db, visible.iter().map(String::as_str));
    let q = [1.0_f64, 0.0];

    mushroomdb::ambiguous_exactness_warns_reset();

    // An unmasked search is unambiguous: approximate is what it has always been.
    db.find_similar_vector("emb", Some("V"), &q, 3, 0.0);
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        0,
        "only a masked call is ambiguous"
    );

    // `exact=True` and a `where=` predicate both say which kernel they want.
    db.find_similar_vector_filtered("emb", Some("V"), &q, 3, 0.0, Some(&mask), None, true)
        .unwrap();
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        0,
        "`exact=True` already names the choice"
    );

    db.find_similar_vector_masked("emb", Some("V"), &q, 3, 0.0, &mask);
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        1,
        "a masked, non-exact search over an indexed field explains itself"
    );

    db.find_similar_vector_masked("emb", Some("V"), &q, 3, 0.0, &mask);
    assert_eq!(
        mushroomdb::ambiguous_exactness_warns(),
        1,
        "once per index — a per-call line would be noise a caller learns to skip"
    );
}

/// A time-travel read through a handle's `Scope` (§5.3, the `query_at` row).
///
/// `AsOfScope` names one restriction — a role, a key list, a namespace, or a
/// role-and-keys pair — and cannot express the general scope a nested
/// `scoped()` builds. This is the entry point the Python child handle needs,
/// and both legs resolve against the **as-of** graph.
#[test]
fn scoped_query_at_answers_the_as_of_graph_through_the_handles_scope() {
    let dir = tmp("query-at-scope");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![]).unwrap();
    db.insert_node("Doc", "hidden", vec![]).unwrap();
    db.insert_node("Doc", "late", vec![]).unwrap();

    let scope = mushroomdb::Scope::new(None, None, Some(vec!["a".into(), "late".into()])).unwrap();
    let params = std::collections::BTreeMap::new();
    let q = "MATCH (n:Doc) RETURN key(n) AS k ORDER BY k";
    let newest = db.wal_total_commits().unwrap() - 1;

    let live = db.query_at_with_scope(newest, q, &params, &scope).unwrap();
    assert_eq!(keys_of(&live), vec!["a", "late"], "`hidden` is never named");

    let then = db.query_at_with_scope(0, q, &params, &scope).unwrap();
    assert_eq!(
        keys_of(&then),
        vec!["a"],
        "the key leg resolves against the graph as it was at that commit"
    );

    // The as-of read leaves the live answer alone: the same scope, resolved
    // again against the live store, still sees both keys.
    let again = db.query_at_with_scope(newest, q, &params, &scope).unwrap();
    assert_eq!(keys_of(&again), vec!["a", "late"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scoped time-travel read is still a read.
#[test]
fn scoped_query_at_refuses_a_write_statement() {
    let dir = tmp("query-at-scope-write");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![]).unwrap();
    let scope = mushroomdb::Scope::new(None, None, Some(vec!["a".into()])).unwrap();
    let params = std::collections::BTreeMap::new();

    let err = db
        .query_at_with_scope(0, "CREATE (n:Doc {id: 'z'})", &params, &scope)
        .expect_err("a write statement on a temporal view is refused");
    assert!(
        matches!(err, GraphError::QueryError { .. }),
        "expected a query error, got {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
