//! Namespaces — the second visibility axis (v0.6.6 §7).
//!
//! A namespace is a reserved, immutable node property (`ns`), not a key prefix:
//! keys are the currency of every surface and `KeyMatch` joins a property value
//! to a key, so prefixing keys would break both. Absent `ns` means the default
//! namespace, so a store that never names one behaves exactly as it did before.
//!
//! Test list (spec §7.9):
//! 1.  `absent_ns_is_the_default_namespace`
//! 2.  `a_namespace_is_set_at_insert_and_immutable`
//! 3.  `invalid_namespace_names_are_refused`
//! 4.  `a_role_sees_only_its_namespaces`
//! 5.  `namespaces_compose_with_visible_where`
//! 6.  `roles_json_version_4_round_trips`
//! 7.  `an_existing_store_is_one_namespace`
//! 8.  `a_scoped_rule_derives_only_inside_its_namespace`
//! 9.  `a_user_edge_may_not_cross_a_namespace`

use core_api::schema::Schema;
use core_api::{
    valid_namespace, GraphDb, GraphError, Predicate, RoleDef, RuleDef, Value, NS_DEFAULT, NS_PROP,
};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-ns-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn ns(name: &str) -> (String, Value) {
    (NS_PROP.to_string(), Value::Str(name.to_string()))
}

// ---------------------------------------------------------------------------
// 1. The model: absent `ns` is the default namespace
// ---------------------------------------------------------------------------

#[test]
fn absent_ns_is_the_default_namespace() {
    let dir = tmp("absent");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "plain", vec![]).unwrap();
    // Writing the default explicitly stores nothing at all.
    db.insert_node("Doc", "explicit", vec![ns(NS_DEFAULT)])
        .unwrap();
    db.insert_node("Doc", "tenant", vec![ns("tenant-a")])
        .unwrap();

    assert_eq!(db.namespace_of("plain").as_deref(), Some(NS_DEFAULT));
    assert_eq!(db.namespace_of("explicit").as_deref(), Some(NS_DEFAULT));
    assert_eq!(db.namespace_of("tenant").as_deref(), Some("tenant-a"));
    assert_eq!(
        db.namespace_of("nobody"),
        None,
        "no live node, no namespace"
    );

    assert_eq!(
        db.get_prop("explicit", NS_PROP),
        None,
        "an explicit default namespace stores no property"
    );
    assert_eq!(
        db.get_prop("tenant", NS_PROP),
        Some(Value::Str("tenant-a".into())),
        "a named namespace is an ordinary string property"
    );

    assert_eq!(
        db.namespaces(),
        vec![NS_DEFAULT.to_string(), "tenant-a".to_string()],
        "every namespace with a live node, in name order"
    );

    // The same answers after a reopen: `node_ns` is derived, never persisted.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.namespace_of("plain").as_deref(), Some(NS_DEFAULT));
    assert_eq!(db.namespace_of("tenant").as_deref(), Some("tenant-a"));
    assert_eq!(
        db.namespaces(),
        vec![NS_DEFAULT.to_string(), "tenant-a".to_string()]
    );

    // Stats carry the per-namespace live counts.
    let stats = db.stats();
    let counted: Vec<(String, usize)> = stats
        .namespaces
        .iter()
        .map(|n| (n.name.clone(), n.nodes_live))
        .collect();
    assert_eq!(
        counted,
        vec![(NS_DEFAULT.to_string(), 2), ("tenant-a".to_string(), 1)]
    );
}

// ---------------------------------------------------------------------------
// 2. Set at insert, immutable after — all three mutation paths
// ---------------------------------------------------------------------------

#[test]
fn a_namespace_is_set_at_insert_and_immutable() {
    let mut db = GraphDb::open(&tmp("immutable")).unwrap();
    db.insert_node(
        "Doc",
        "a",
        vec![("id".into(), Value::Str("a".into())), ns("x")],
    )
    .unwrap();

    // (a) set_prop
    match db.set_prop("a", NS_PROP, Value::Str("y".into())) {
        Err(GraphError::NamespaceImmutable { key, from, to }) => {
            assert_eq!((key.as_str(), from.as_str(), to.as_str()), ("a", "x", "y"));
        }
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));

    // (b) Cypher SET
    match db.query_write(
        "MATCH (n:Doc) WHERE n.id = 'a' SET n.ns = 'y'",
        &no_params(),
    ) {
        Err(GraphError::NamespaceImmutable { from, to, .. }) => {
            assert_eq!((from.as_str(), to.as_str()), ("x", "y"));
        }
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));

    // (c) the merge path every upsert uses — a batched SetProp
    let mut batch = db.batch();
    batch.set_prop("a", NS_PROP, Value::Str("y".into()));
    match batch.commit() {
        Err(GraphError::NamespaceImmutable { from, to, .. }) => {
            assert_eq!((from.as_str(), to.as_str()), ("x", "y"));
        }
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    assert_eq!(
        db.get_prop("a", NS_PROP),
        Some(Value::Str("x".into())),
        "the node is unchanged after every refusal"
    );

    // Writing the namespace it already has is a no-op, not an error — and a true
    // no-op: it takes no commit and writes no WAL record.
    let before = db.commit_seq();
    db.set_prop("a", NS_PROP, Value::Str("x".into())).unwrap();
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    assert_eq!(db.commit_seq(), before, "a no-op ns write is not a commit");
    let mut batch = db.batch();
    batch.set_prop("a", NS_PROP, Value::Str("x".into()));
    assert_eq!(batch.commit().unwrap(), (0, 0));
    assert_eq!(db.commit_seq(), before, "nor is a batch of only no-ops");

    // Same for a default-namespace node written the long way round.
    db.insert_node("Doc", "d", vec![("id".into(), Value::Str("d".into()))])
        .unwrap();
    db.set_prop("d", NS_PROP, Value::Str(NS_DEFAULT.into()))
        .unwrap();
    assert_eq!(db.namespace_of("d").as_deref(), Some(NS_DEFAULT));
    assert_eq!(
        db.get_prop("d", NS_PROP),
        None,
        "the no-op write stores nothing"
    );
    match db.set_prop("d", NS_PROP, Value::Str("y".into())) {
        Err(GraphError::NamespaceImmutable { from, to, .. }) => {
            assert_eq!((from.as_str(), to.as_str()), (NS_DEFAULT, "y"));
        }
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }

    // rename_node changes the key, not the namespace.
    db.rename_node("a", "a2").unwrap();
    assert_eq!(db.namespace_of("a2").as_deref(), Some("x"));
}

// ---------------------------------------------------------------------------
// 3. Invalid namespace names
// ---------------------------------------------------------------------------

#[test]
fn invalid_namespace_names_are_refused() {
    assert!(valid_namespace("a"));
    assert!(valid_namespace(NS_DEFAULT));
    assert!(valid_namespace("tenant-a.1_2"));
    assert!(valid_namespace(&"n".repeat(64)));
    assert!(!valid_namespace(""));
    assert!(!valid_namespace(&"n".repeat(65)));
    assert!(!valid_namespace("a/b"));
    assert!(!valid_namespace("a b"));

    let mut db = GraphDb::open(&tmp("invalid")).unwrap();
    for bad in ["", &"n".repeat(65), "a/b"] {
        let err = db
            .insert_node("Doc", "bad", vec![ns(bad)])
            .expect_err("an invalid namespace name must be refused");
        assert!(
            matches!(err, GraphError::RuleInvalid { .. }),
            "expected RuleInvalid, got {err:?}"
        );
        assert!(!db.has_node("bad"), "nothing was written");
    }
    // A non-string `ns` is refused too.
    let err = db
        .insert_node("Doc", "bad", vec![(NS_PROP.into(), Value::Int(7))])
        .expect_err("a non-string namespace must be refused");
    assert!(matches!(err, GraphError::RuleInvalid { .. }));
}

// ---------------------------------------------------------------------------
// 4. A role bound to a namespace sees nothing outside it
// ---------------------------------------------------------------------------

fn role(name: &str, labels: &[&str], namespaces: Option<&[&str]>) -> RoleDef {
    RoleDef {
        name: name.into(),
        keys: vec![],
        labels: labels.iter().map(|s| s.to_string()).collect(),
        visible_where: None,
        namespaces: namespaces.map(|ns| ns.iter().map(|s| s.to_string()).collect()),
        write: None,
    }
}

/// Keys a role can actually read, via the masked query path, sorted.
fn role_keys(db: &GraphDb<core_api::RealFs>, name: &str) -> Vec<String> {
    let mask = db.mask_for_role(name).unwrap();
    let rs = db
        .query_masked("MATCH (n) RETURN n", &no_params(), &mask)
        .unwrap();
    let mut keys: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.row(i)[0].as_ref() {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    keys.sort();
    keys
}

/// Keys the MVCC reader's twin resolver gives the same role, sorted.
fn reader_role_keys(db: &GraphDb<core_api::RealFs>, name: &str) -> Vec<String> {
    let reader = db.reader();
    let mask = reader.mask_for_role(name).unwrap();
    let rs = reader
        .query_masked("MATCH (n) RETURN n", &no_params(), &mask)
        .unwrap();
    let mut keys: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.row(i)[0].as_ref() {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    keys.sort();
    keys
}

#[test]
fn a_role_sees_only_its_namespaces() {
    let dir = tmp("role-ns");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Document", "a", vec![ns("x")]).unwrap();
    db.insert_node("Document", "b", vec![ns("y")]).unwrap();
    db.insert_node("Document", "plain", vec![]).unwrap();

    db.apply_schema(&Schema {
        roles: vec![
            role("x-only", &["Document"], Some(&["x"])),
            role("unscoped", &["Document"], None),
            role("two", &["Document"], Some(&["x", "default"])),
        ],
        ..Default::default()
    })
    .unwrap();

    assert_eq!(role_keys(&db, "x-only"), vec!["a"]);
    assert_eq!(role_keys(&db, "unscoped"), vec!["a", "b", "plain"]);
    assert_eq!(role_keys(&db, "two"), vec!["a", "plain"]);

    // The snapshot reader's twin resolver agrees, node for node.
    for name in ["x-only", "unscoped", "two"] {
        assert_eq!(
            reader_role_keys(&db, name),
            role_keys(&db, name),
            "the reader resolver must agree with the live one for {name}"
        );
    }

    // The namespace leg intersects `keys` too, so a key in another namespace is
    // a mistake `apply_schema` names rather than an administrative grant.
    let mut bad = role("x-only", &["Document"], Some(&["x"]));
    bad.keys = vec!["b".into()];
    let err = db
        .apply_schema(&Schema {
            roles: vec![bad],
            ..Default::default()
        })
        .expect_err("a key outside the role's namespaces must be refused");
    let msg = err.to_string();
    assert!(msg.contains('b') && msg.contains('y'), "message: {msg}");

    // A key naming no live node is still silently ignored.
    let mut ghost = role("ghosts", &["Document"], Some(&["x"]));
    ghost.keys = vec!["nobody".into()];
    db.apply_schema(&Schema {
        roles: vec![ghost],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(role_keys(&db, "ghosts"), vec!["a"]);

    // `namespaces: []` is refused: a role that sees nothing omits keys+labels.
    let empty = RoleDef {
        namespaces: Some(vec![]),
        ..role("empty", &["Document"], None)
    };
    let err = db
        .apply_schema(&Schema {
            roles: vec![empty],
            ..Default::default()
        })
        .expect_err("namespaces: [] must be refused");
    assert!(err.to_string().contains("see nothing"), "{err}");

    // An invalid namespace name on a role is refused.
    let err = db
        .apply_schema(&Schema {
            roles: vec![role("slash", &["Document"], Some(&["a/b"]))],
            ..Default::default()
        })
        .expect_err("an invalid namespace name must be refused");
    assert!(err.to_string().contains("a/b"), "{err}");
}

// ---------------------------------------------------------------------------
// 5. Namespaces compose with a predicate mask
// ---------------------------------------------------------------------------

#[test]
fn namespaces_compose_with_visible_where() {
    let dir = tmp("compose");
    let mut db = GraphDb::open(&dir).unwrap();
    // label × status × namespace: only `hit` satisfies all three.
    let published = || ("status".to_string(), Value::Str("published".into()));
    let draft = || ("status".to_string(), Value::Str("draft".into()));
    db.insert_node("Doc", "hit", vec![published(), ns("x")])
        .unwrap();
    db.insert_node("Doc", "wrong-ns", vec![published(), ns("y")])
        .unwrap();
    db.insert_node("Doc", "wrong-status", vec![draft(), ns("x")])
        .unwrap();
    db.insert_node("Other", "wrong-label", vec![published(), ns("x")])
        .unwrap();

    let pred = core_api::PropPredicate {
        field: "status".into(),
        eq: Some(Value::Str("published".into())),
        in_: None,
    };
    let all_three = RoleDef {
        visible_where: Some(pred.clone()),
        ..role("all-three", &["Doc"], Some(&["x"]))
    };
    let no_ns = RoleDef {
        name: "no-ns".into(),
        visible_where: Some(pred.clone()),
        ..role("no-ns", &["Doc"], None)
    };
    let no_pred = role("no-pred", &["Doc"], Some(&["x"]));
    let more_labels = RoleDef {
        visible_where: Some(pred),
        ..role("more-labels", &["Doc", "Other"], Some(&["x"]))
    };
    db.apply_schema(&Schema {
        roles: vec![all_three, no_ns, no_pred, more_labels],
        ..Default::default()
    })
    .unwrap();

    assert_eq!(role_keys(&db, "all-three"), vec!["hit"]);
    assert_eq!(
        role_keys(&db, "no-ns"),
        vec!["hit", "wrong-ns"],
        "dropping the namespace leg grows the set"
    );
    assert_eq!(
        role_keys(&db, "no-pred"),
        vec!["hit", "wrong-status"],
        "dropping the predicate grows the set"
    );
    assert_eq!(
        role_keys(&db, "more-labels"),
        vec!["hit", "wrong-label"],
        "widening the labels grows the set"
    );
    for name in ["all-three", "no-ns", "no-pred", "more-labels"] {
        assert_eq!(reader_role_keys(&db, name), role_keys(&db, name), "{name}");
    }
}

// ---------------------------------------------------------------------------
// 6. roles.json version 4
// ---------------------------------------------------------------------------

fn roles_version(dir: &std::path::Path) -> u64 {
    let bytes = std::fs::read(dir.join("roles.json")).expect("roles.json");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    parsed["version"].as_u64().unwrap()
}

#[test]
fn roles_json_version_4_round_trips() {
    let dir = tmp("v4");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![ns("x")]).unwrap();
    db.insert_node("Doc", "b", vec![ns("y")]).unwrap();

    // A role with no namespaces still writes the version 0.6.5 would have.
    db.apply_schema(&Schema {
        roles: vec![role("plain", &["Doc"], None)],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        roles_version(&dir),
        1,
        "no write scope, no predicate, no ns"
    );

    db.apply_schema(&Schema {
        roles: vec![role("scoped", &["Doc"], Some(&["x"]))],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        roles_version(&dir),
        4,
        "a namespace binding writes version 4"
    );

    // Reopen: the sidecar round-trips and resolves identically.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(role_keys(&db, "scoped"), vec!["a"]);
    assert_eq!(role_keys(&db, "plain"), vec!["a", "b"]);
    let reloaded = db
        .roles()
        .into_iter()
        .find(|r| r.name == "scoped")
        .expect("scoped role");
    assert_eq!(reloaded.namespaces.as_deref(), Some(&["x".to_string()][..]));
    drop(db);

    // Version 5 and up still poison: never widen on a version this binary
    // does not understand.
    std::fs::write(
        dir.join("roles.json"),
        br#"{"version":5,"roles":[{"name":"scoped","labels":["Doc"]}]}"#,
    )
    .unwrap();
    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.mask_for_role("scoped").is_err(),
        "a version-5 sidecar must poison the roles state"
    );
}

// ---------------------------------------------------------------------------
// 7. Migration: an existing store is one implicit default namespace
// ---------------------------------------------------------------------------

#[test]
fn an_existing_store_is_one_namespace() {
    let dir = tmp("golden");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("snapshot.bin"),
        include_bytes!("fixtures/golden_v9.bin"),
    )
    .unwrap();
    std::fs::write(dir.join("wal.bin"), b"").unwrap();

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.node_count(), 2, "the fixture is the committed V9 store");
    assert_eq!(db.namespaces(), vec![NS_DEFAULT.to_string()]);
    for key in ["a", "b"] {
        assert_eq!(db.namespace_of(key).as_deref(), Some(NS_DEFAULT));
        assert_eq!(db.get_prop(key, NS_PROP), None, "no ns property exists");
    }

    // A role with no namespaces sees exactly what it saw before.
    let label = db.node_info("a").expect("node a").label;
    db.apply_schema(&Schema {
        roles: vec![role("all", &[&label], None)],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(role_keys(&db, "all"), vec!["a", "b"]);
    assert_eq!(
        roles_version(&dir),
        1,
        "no role carries a namespace, so the sidecar version is unchanged"
    );
    // An edge inside the one implicit namespace still inserts.
    assert!(db.insert_edge("E2", "a", "b").unwrap());
}

// ---------------------------------------------------------------------------
// 8. A scoped rule derives only inside its namespace
// ---------------------------------------------------------------------------

fn tag_rule(name: &str, namespace: Option<&str>) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "TAGGED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: namespace.map(str::to_string),
    }
}

/// Every `TAGGED` pair in the store, sorted.
fn tagged_pairs(db: &GraphDb<core_api::RealFs>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for src in ["p1", "p2", "p3"] {
        for dst in db
            .neighbors(src, "TAGGED", core_api::Direction::Out)
            .unwrap()
        {
            out.push((src.to_string(), dst));
        }
    }
    out.sort();
    out
}

/// Six nodes that all satisfy the predicate, three in `x` and three in `y`.
fn two_tenant_store(dir: &std::path::Path) -> GraphDb<core_api::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    let tag = || ("tag".to_string(), Value::Str("t".into()));
    db.insert_node("Person", "p1", vec![tag(), ns("x")])
        .unwrap();
    db.insert_node("Person", "p2", vec![tag(), ns("x")])
        .unwrap();
    db.insert_node("Person", "p3", vec![tag(), ns("y")])
        .unwrap();
    db.insert_node("Org", "o1", vec![tag(), ns("x")]).unwrap();
    db.insert_node("Org", "o2", vec![tag(), ns("y")]).unwrap();
    db.insert_node("Org", "o3", vec![tag(), ns("y")]).unwrap();
    db
}

#[test]
fn a_scoped_rule_derives_only_inside_its_namespace() {
    let dir = tmp("scoped-rule");
    let mut db = two_tenant_store(&dir);

    // Scoped to `x`: only the x nodes enter the rule, so every derived edge is
    // intra-namespace by construction.
    db.create_rule(tag_rule("scoped", Some("x"))).unwrap();
    assert_eq!(
        tagged_pairs(&db),
        vec![("p1".into(), "o1".into()), ("p2".into(), "o1".into())],
        "a rule scoped to x derives only the x pairs"
    );

    // A node arriving later in another namespace changes nothing.
    db.insert_node(
        "Org",
        "o4",
        vec![("tag".into(), Value::Str("t".into())), ns("y")],
    )
    .unwrap();
    assert_eq!(
        tagged_pairs(&db),
        vec![("p1".into(), "o1".into()), ("p2".into(), "o1".into())],
    );

    // The same rule, global: every pair, the crossing ones included.
    db.delete_rule("scoped").unwrap();
    db.create_rule(tag_rule("global", None)).unwrap();
    let pairs = tagged_pairs(&db);
    assert!(
        pairs.contains(&("p1".to_string(), "o2".to_string()))
            && pairs.contains(&("p3".to_string(), "o1".to_string())),
        "a global rule may derive across namespaces: {pairs:?}"
    );
    assert_eq!(pairs.len(), 12, "3 persons × 4 orgs: {pairs:?}");

    // Scoped again, then a reopen: replay re-derives exactly the same set.
    db.delete_rule("global").unwrap();
    db.create_rule(tag_rule("scoped", Some("y"))).unwrap();
    let scoped_y = tagged_pairs(&db);
    assert_eq!(
        scoped_y,
        vec![
            ("p3".into(), "o2".into()),
            ("p3".into(), "o3".into()),
            ("p3".into(), "o4".into())
        ]
    );
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(tagged_pairs(&db), scoped_y, "replay derives the same set");
}

/// A via-hop rule is scoped by the same check: the via node is a node.
#[test]
fn a_scoped_via_rule_never_hops_out_of_its_namespace() {
    let dir = tmp("scoped-via");
    let mut db = two_tenant_store(&dir);
    // p1 (x) → p3 (y) is impossible as a user edge, so the hop that would leave
    // the namespace goes through an Org in x and another in y.
    db.insert_node("Hub", "h-x", vec![ns("x")]).unwrap();
    db.insert_node("Hub", "h-y", vec![ns("y")]).unwrap();
    db.insert_edge("VIA", "p1", "h-x").unwrap();
    db.insert_edge("VIA", "p3", "h-y").unwrap();

    let mut rule = tag_rule("via-scoped", Some("x"));
    rule.via_label = Some("Hub".into());
    rule.via_edge = Some("VIA".into());
    rule.predicate = Predicate::FieldEqual {
        field: "tag".into(),
    };
    // The via node carries the tag so the via→dst predicate can hold.
    db.set_prop("h-x", "tag", Value::Str("t".into())).unwrap();
    db.set_prop("h-y", "tag", Value::Str("t".into())).unwrap();
    db.create_rule(rule).unwrap();

    assert_eq!(
        tagged_pairs(&db),
        vec![("p1".into(), "o1".into())],
        "the scoped via rule stays inside x"
    );
}

// ---------------------------------------------------------------------------
// 9. No cross-namespace user edge
// ---------------------------------------------------------------------------

#[test]
fn a_user_edge_may_not_cross_a_namespace() {
    let dir = tmp("cross-edge");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "x1", vec![ns("x")]).unwrap();
    db.insert_node("Doc", "x2", vec![ns("x")]).unwrap();
    db.insert_node("Doc", "y1", vec![ns("y")]).unwrap();
    db.insert_node("Doc", "plain", vec![]).unwrap();

    match db.insert_edge("LINKS", "x1", "y1") {
        Err(GraphError::CrossNamespace {
            src,
            src_ns,
            dst,
            dst_ns,
        }) => assert_eq!(
            (src.as_str(), src_ns.as_str(), dst.as_str(), dst_ns.as_str()),
            ("x1", "x", "y1", "y")
        ),
        other => panic!("expected CrossNamespace, got {other:?}"),
    }
    // The default namespace is a namespace like any other.
    assert!(matches!(
        db.insert_edge("LINKS", "plain", "x1"),
        Err(GraphError::CrossNamespace { .. })
    ));
    // Inside either namespace the same edge succeeds.
    assert!(db.insert_edge("LINKS", "x1", "x2").unwrap());
    assert!(db.insert_edge("LINKS", "y1", "y1").unwrap());

    // A batch refuses it too, and writes nothing.
    let mut batch = db.batch();
    batch.insert_edge("LINKS", "x2", "y1");
    assert!(matches!(
        batch.commit(),
        Err(GraphError::CrossNamespace { .. })
    ));
    assert!(db
        .node_edges("x2")
        .unwrap()
        .iter()
        .all(|e| e.dst_key != "y1"));

    // A node created in the same batch as the edge is measured by the namespace
    // it is being created in.
    let mut batch = db.batch();
    batch.insert_node("Doc", "x3", vec![ns("x")]);
    batch.insert_edge("LINKS", "x3", "y1");
    assert!(matches!(
        batch.commit(),
        Err(GraphError::CrossNamespace { .. })
    ));
    assert!(!db.has_node("x3"), "the whole batch was refused");

    let mut batch = db.batch();
    batch.insert_node("Doc", "x3", vec![ns("x")]);
    batch.insert_edge("LINKS", "x3", "x1");
    batch.commit().unwrap();
    assert_eq!(db.namespace_of("x3").as_deref(), Some("x"));
}

// ---------------------------------------------------------------------------
// 10. The derived array survives a snapshot, and a delete empties a namespace
// ---------------------------------------------------------------------------

/// `node_ns` is rebuilt from the `ns` column at open, so a store whose nodes live
/// in the mmap'd snapshot base resolves exactly as a WAL-only one does — in both
/// resolvers.
#[test]
fn namespaces_survive_a_snapshot_and_a_delete() {
    let dir = tmp("snapshot-ns");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Document", "a", vec![ns("x")]).unwrap();
    db.insert_node("Document", "b", vec![ns("y")]).unwrap();
    db.insert_node("Document", "plain", vec![]).unwrap();
    db.apply_schema(&Schema {
        roles: vec![
            role("x-only", &["Document"], Some(&["x"])),
            role("y-only", &["Document"], Some(&["y"])),
        ],
        ..Default::default()
    })
    .unwrap();
    db.snapshot().unwrap();
    drop(db);

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.namespaces(),
        vec![NS_DEFAULT.to_string(), "x".to_string(), "y".to_string()]
    );
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    assert_eq!(role_keys(&db, "x-only"), vec!["a"]);
    assert_eq!(reader_role_keys(&db, "x-only"), vec!["a"]);
    assert_eq!(role_keys(&db, "y-only"), vec!["b"]);
    assert_eq!(reader_role_keys(&db, "y-only"), vec!["b"]);

    // A node inserted after the snapshot joins the namespace it names.
    db.insert_node("Document", "a2", vec![ns("x")]).unwrap();
    assert_eq!(role_keys(&db, "x-only"), vec!["a", "a2"]);
    assert_eq!(reader_role_keys(&db, "x-only"), vec!["a", "a2"]);

    // Deleting the last node of a namespace takes the namespace with it.
    db.delete_node("b").unwrap();
    assert_eq!(db.namespace_of("b"), None);
    assert_eq!(
        db.namespaces(),
        vec![NS_DEFAULT.to_string(), "x".to_string()],
        "y has no live node left"
    );
    assert!(db.stats().namespaces.iter().all(|n| n.name != "y"));
    assert!(role_keys(&db, "y-only").is_empty());
    assert!(reader_role_keys(&db, "y-only").is_empty());
}

// ---------------------------------------------------------------------------
// 11. Removing `ns` is changing it, and is refused the same way
// ---------------------------------------------------------------------------

/// `RemoveProp` needs no dense rewrite, so it does not pass the seam that
/// refuses a namespace change — `prepare_remove_prop` has to say so itself.
/// Without that, removing `ns` lands the node in `default` on the next open:
/// the cross-namespace edge guard is defeated and a default-bound role reads a
/// tenant's node.
#[test]
fn removing_the_ns_property_is_refused() {
    let dir = tmp("remove-ns");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![ns("x")]).unwrap();
    db.insert_node("Doc", "plain", vec![]).unwrap();

    // (a) remove_prop
    match db.remove_prop("a", NS_PROP) {
        Err(GraphError::NamespaceImmutable { key, from, to }) => assert_eq!(
            (key.as_str(), from.as_str(), to.as_str()),
            ("a", "x", NS_DEFAULT)
        ),
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }

    // (b) a batched RemoveProp, which refuses the whole batch
    let mut batch = db.batch();
    batch.remove_prop("a", NS_PROP);
    match batch.commit() {
        Err(GraphError::NamespaceImmutable { from, to, .. }) => {
            assert_eq!((from.as_str(), to.as_str()), ("x", NS_DEFAULT));
        }
        other => panic!("expected NamespaceImmutable, got {other:?}"),
    }

    // (c) Cypher REMOVE, if the dialect reaches it, must not be a way round.
    if let Ok(_rs) = db.query_write("MATCH (n:Doc) WHERE n.id = 'a' REMOVE n.ns", &no_params()) {
        assert_eq!(
            db.namespace_of("a").as_deref(),
            Some("x"),
            "no Cypher path may strip the namespace"
        );
    }

    // The node is untouched, and stays untouched across a reopen — this is the
    // assertion that would have caught the silent move to `default`.
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    assert_eq!(db.get_prop("a", NS_PROP), Some(Value::Str("x".into())));
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.namespace_of("a").as_deref(), Some("x"));
    assert_eq!(
        db.namespaces(),
        vec![NS_DEFAULT.to_string(), "x".to_string()]
    );
    // And the edge guard still holds, which is what the refusal protects.
    assert!(matches!(
        db.insert_edge("LINKS", "a", "plain"),
        Err(GraphError::CrossNamespace { .. })
    ));

    // Removing `ns` from a node already in the default namespace is the same
    // no-op that setting it to `default` is — there is no namespace to change.
    let before = db.commit_seq();
    assert!(!db.remove_prop("plain", NS_PROP).unwrap());
    assert_eq!(db.commit_seq(), before);
    assert_eq!(db.namespace_of("plain").as_deref(), Some(NS_DEFAULT));
}

// ---------------------------------------------------------------------------
// 12. A global rule's crossing edge is invisible to a namespaced role
// ---------------------------------------------------------------------------

/// A global rule may derive across a boundary. A role bound to one namespace
/// must still never see the node on the other side of such an edge — role masks
/// are Omit-mode, so the edge goes with it.
#[test]
fn a_global_rules_crossing_edge_is_invisible_to_a_namespaced_role() {
    let dir = tmp("crossing-edge-masked");
    let mut db = two_tenant_store(&dir);
    db.create_rule(tag_rule("global", None)).unwrap();
    // The rule really does cross: p1 (x) → o2 (y).
    assert!(db
        .neighbors("p1", "TAGGED", core_api::Direction::Out)
        .unwrap()
        .contains(&"o2".to_string()));

    db.apply_schema(&Schema {
        roles: vec![
            role("x-only", &["Person", "Org"], Some(&["x"])),
            role("unscoped", &["Person", "Org"], None),
        ],
        ..Default::default()
    })
    .unwrap();

    // Cypher: no row naming a node outside the role's namespace.
    let mask = db.mask_for_role("x-only").unwrap();
    let rs = db
        .query_masked("MATCH (a)-[:TAGGED]->(b) RETURN a, b", &no_params(), &mask)
        .unwrap();
    let pairs: Vec<(String, String)> = (0..rs.len())
        .filter_map(|i| {
            let row = rs.row(i);
            match (row[0].as_ref(), row[1].as_ref()) {
                (Some(Value::Str(a)), Some(Value::Str(b))) => Some((a.clone(), b.clone())),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("p1".to_string(), "o1".to_string()),
            ("p2".to_string(), "o1".to_string())
        ],
        "only the intra-x pairs survive: every crossing edge went with its \
         hidden endpoint: {pairs:?}"
    );

    // node_edges under the mask: Omit, so no stub and no foreign key leaks.
    let edges = db.node_edges_masked("p1", &mask).unwrap();
    let dsts: Vec<String> = edges.iter().map(|e| e.dst_key.clone()).collect();
    assert_eq!(dsts, vec!["o1".to_string()], "masked node_edges: {dsts:?}");
    assert!(
        edges.iter().all(|e| e.src_key == "p1" && e.derived),
        "the surviving edge is the derived intra-namespace one"
    );

    // The unscoped role still sees the crossing edge — the rule is global.
    let wide = db.mask_for_role("unscoped").unwrap();
    let wide_dsts: Vec<String> = db
        .node_edges_masked("p1", &wide)
        .unwrap()
        .iter()
        .map(|e| e.dst_key.clone())
        .collect();
    assert!(
        wide_dsts.contains(&"o2".to_string()),
        "an unscoped role sees what the global rule derived: {wide_dsts:?}"
    );
}

// ---------------------------------------------------------------------------
// 13. A view may not own the namespace column
// ---------------------------------------------------------------------------

#[test]
fn a_view_may_not_write_the_namespace_property() {
    let dir = tmp("view-ns");
    let mut db = GraphDb::open(&dir).unwrap();
    let err = db
        .create_view(core_api::ViewDef {
            name: "ns-view".into(),
            label: "Doc".into(),
            view_prop: NS_PROP.into(),
            source: core_api::ViewSource::Degree {
                edge_type: "LINKS".into(),
                direction: core_api::Direction::Out,
            },
        })
        .expect_err("a view over the reserved namespace property must be refused");
    assert!(
        err.to_string().contains("reserved namespace property"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// 14. A namespaced VectorSimilar rule, which finds candidates through the index
// ---------------------------------------------------------------------------

/// From 0.6.6 a `VectorSimilar` rule — exact as well as approximate — finds its
/// candidates through the vector index rather than by scanning. Two things have
/// to hold for a scoped one: the index holds only its own namespace (so the
/// beam's floor proof is over the right corpus), and no pair crosses.
///
/// The vectors are identical across namespaces, so every pair would match at
/// `min = 0.9`: a rule that ignored the scoping would derive the full
/// cross-product and the count is what says it did not.
#[test]
fn a_namespaced_vector_rule_never_pairs_across_namespaces() {
    fn vector_rule(name: &str, namespace: Option<&str>, approximate: bool) -> RuleDef {
        RuleDef {
            name: name.into(),
            src_label: "Vec".into(),
            dst_label: "Vec".into(),
            predicate: Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            edge_type: "NEAR".into(),
            weight_prop: None,
            max_edges: None,
            approximate,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: namespace.map(str::to_string),
        }
    }
    /// Every `NEAR` pair over the fixture, sorted.
    fn near_pairs(db: &GraphDb<core_api::RealFs>, keys: &[&str]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for src in keys {
            for dst in db
                .neighbors(src, "NEAR", core_api::Direction::Out)
                .unwrap_or_default()
            {
                out.push((src.to_string(), dst));
            }
        }
        out.sort();
        out
    }

    let keys = ["vx1", "vx2", "vx3", "vy1", "vy2", "vy3"];
    for approximate in [false, true] {
        let dir = tmp(if approximate {
            "vec-ns-approx"
        } else {
            "vec-ns-exact"
        });
        let mut db = GraphDb::open(&dir).unwrap();
        // Six nodes, three per namespace, all with near-identical unit vectors,
        // so every ordered pair clears min = 0.9.
        for (i, key) in keys.iter().enumerate() {
            let nudge = i as f64 * 1e-4;
            let emb = Value::List(vec![
                Value::Float(1.0 - nudge),
                Value::Float(nudge),
                Value::Float(0.0),
            ]);
            let namespace = if key.starts_with("vx") { "x" } else { "y" };
            db.insert_node("Vec", key, vec![("emb".into(), emb), ns(namespace)])
                .unwrap();
        }

        db.create_rule(vector_rule("scoped", Some("x"), approximate))
            .unwrap();
        let pairs = near_pairs(&db, &keys);
        assert_eq!(
            pairs.len(),
            6,
            "3 x-nodes, every ordered pair but a self-pair (approximate={approximate}): {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .all(|(s, d)| s.starts_with("vx") && d.starts_with("vx")),
            "no pair may touch namespace y (approximate={approximate}): {pairs:?}"
        );

        // A node arriving in the other namespace reaches neither side of the
        // index, so it changes nothing — this is the incremental path, which
        // files the vector through a different entry point than the backfill.
        let emb = Value::List(vec![
            Value::Float(1.0),
            Value::Float(0.0),
            Value::Float(0.0),
        ]);
        db.insert_node("Vec", "vy4", vec![("emb".into(), emb), ns("y")])
            .unwrap();
        assert_eq!(
            near_pairs(&db, &["vy4"]),
            Vec::<(String, String)>::new(),
            "a y node derives nothing under an x-scoped rule"
        );
        assert_eq!(near_pairs(&db, &keys).len(), 6);

        // The same rule, global: now every pair crosses freely — 7 nodes.
        db.delete_rule("scoped").unwrap();
        db.create_rule(vector_rule("global", None, approximate))
            .unwrap();
        let all_keys = ["vx1", "vx2", "vx3", "vy1", "vy2", "vy3", "vy4"];
        let global = near_pairs(&db, &all_keys);
        assert_eq!(
            global.len(),
            42,
            "7 nodes × 6 others (approximate={approximate}): {}",
            global.len()
        );
        assert!(
            global.contains(&("vx1".to_string(), "vy1".to_string())),
            "a global vector rule may cross the boundary"
        );
    }
}
