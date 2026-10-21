//! Unique constraints (0.10 TY-5).
//!
//! A unique constraint on `(label, field)` refuses any write that would leave
//! two live nodes of `label` with equal values for `field`. The check runs at
//! the one function every WAL append passes through — the dense rewrite — so
//! every write path gets it, and these tests prove it path by path: the write
//! fails with `ConstraintViolation`, `wal.bin` is byte-for-byte unchanged, and
//! the store reads the same live as after a reopen.
//!
//! `CreateConstraint` / `DropConstraint` are format-V11 WAL records, which a
//! 0.9.x reader cannot decode. Until the V11 format lands (TY-12) the write
//! path refuses them like a V11 value, so a default build cannot declare a
//! constraint at all — the first test below holds that. Everything else runs
//! on a handle the `test-hooks` hook unlocks, which is the path TY-12 opens
//! for real: `cargo test -p mushroomdb --features test-hooks --test constraints`.

use mushroomdb::{GraphError, RealFs, Value};
use std::path::{Path, PathBuf};

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-constraints-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ))
}

fn wal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

const CONSTRAINT_REFUSAL: &str = "not supported in mushroomdb 0.10 yet: unique constraints \
                                  cannot be declared until the V11 format lands";

#[test]
fn create_constraint_is_refused_until_the_v11_format_lands() {
    let dir = tmp("locked");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "User",
        "alice",
        vec![("email".into(), Value::Str("a@x".into()))],
    )
    .unwrap();
    let before = wal_bytes(&dir);
    match db.create_constraint("uniq_email", "User", "email") {
        Err(GraphError::QueryError { detail }) => assert_eq!(detail, CONSTRAINT_REFUSAL),
        other => panic!("expected the V11 record refusal, got {other:?}"),
    }
    assert_eq!(
        wal_bytes(&dir),
        before,
        "a refused declaration was appended"
    );
    assert!(db.constraints().is_empty());
    assert!(
        !db.is_index_enabled("User", "email"),
        "the backing index comes with the record, so a refused record leaves none"
    );
    // Nothing is enforced: the declaration never happened.
    db.insert_node(
        "User",
        "bob",
        vec![("email".into(), Value::Str("a@x".into()))],
    )
    .unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.constraints().is_empty());
    assert_eq!(reopened.node_count(), 2);
}

#[cfg(feature = "test-hooks")]
mod unlocked {
    use super::*;
    use mushroomdb::{
        BatchBuilder, BatchOp, Direction, IngestOptions, OnConflict, Precondition, PropPredicate,
        RoleDef, SharedDb, SnapshotOptions, UniqueConstraint, ViewDef, ViewSource, WriteAuthz,
        WriteScope, BINARY_MAX_BYTES,
    };
    use std::collections::BTreeMap;

    type Batch<'a> = BatchBuilder<'a, RealFs>;

    const VIOLATION: &str = "constraint violation: property 'email' is declared unique by \
                             constraint 'uniq_email'; a node with that value already exists";

    fn open_temp(name: &str) -> (PathBuf, GraphDb) {
        let dir = tmp(name);
        let mut db = GraphDb::open(&dir).unwrap();
        db.unlock_v11_format_for_test();
        (dir, db)
    }

    fn email(s: &str) -> Vec<(String, Value)> {
        vec![("email".into(), Value::Str(s.into()))]
    }

    /// A store with `uniq_email` on `(User, email)` and alice holding `a@x`.
    fn constrained(name: &str) -> (PathBuf, GraphDb) {
        let (dir, mut db) = open_temp(name);
        db.create_constraint("uniq_email", "User", "email").unwrap();
        db.insert_node("User", "alice", email("a@x")).unwrap();
        (dir, db)
    }

    fn assert_violation<T: std::fmt::Debug>(result: Result<T, GraphError>) {
        match result {
            Err(e @ GraphError::ConstraintViolation { .. }) => {
                let GraphError::ConstraintViolation {
                    ref constraint,
                    ref field,
                } = e
                else {
                    unreachable!()
                };
                assert_eq!(constraint, "uniq_email");
                assert_eq!(field, "email");
                assert_eq!(e.to_string(), VIOLATION);
            }
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    /// After a refused write: nothing appended, alice is the only holder, and a
    /// reopened store answers exactly as the never-closed one does.
    fn assert_nothing_written(dir: &Path, db: GraphDb, wal_before: &[u8]) {
        assert_eq!(
            wal_bytes(dir),
            wal_before,
            "a refused write appended to the WAL"
        );
        let alice = db.node_info("alice").expect("alice survives");
        assert_eq!(alice.props.get("email"), Some(&Value::Str("a@x".into())));
        assert!(db.node_info("bob").is_none(), "bob must not exist");
        let count = db.node_count();
        drop(db);
        let reopened = GraphDb::open(dir).unwrap();
        assert_eq!(reopened.node_info("alice"), Some(alice));
        assert!(reopened.node_info("bob").is_none());
        assert_eq!(reopened.node_count(), count);
    }

    // ── The brief's cases ────────────────────────────────────────────────────

    #[test]
    fn test_constraint_single_insert_succeeds() {
        let (_dir, mut db) = open_temp("single");
        db.create_constraint("uniq_email", "User", "email").unwrap();
        db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("User", "alice", email("alice@x.com"));
        })
        .unwrap();
        assert_eq!(
            db.constraints(),
            &[UniqueConstraint {
                name: "uniq_email".into(),
                label: "User".into(),
                field: "email".into(),
            }]
        );
        assert!(
            db.is_index_enabled("User", "email"),
            "the backing index is created"
        );
    }

    #[test]
    fn test_constraint_duplicate_cross_batch() {
        let (dir, mut db) = constrained("cross-batch");
        let before = wal_bytes(&dir);
        assert_violation(db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("User", "bob", email("a@x"));
        }));
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn test_constraint_duplicate_within_batch() {
        let (dir, mut db) = open_temp("within-batch");
        db.create_constraint("uniq_email", "User", "email").unwrap();
        db.insert_node("User", "alice", email("a@x")).unwrap();
        let before = wal_bytes(&dir);
        assert_violation(db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("User", "carol", email("dup@x.com"));
            b.insert_node("User", "bob", email("dup@x.com"));
        }));
        assert!(
            db.node_info("carol").is_none(),
            "the whole batch is refused"
        );
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn test_unique_constraint_datetime_offset_equality() {
        // Review Focus #1: same UTC instant, different offset → conflict.
        let (_dir, mut db) = open_temp("datetime");
        db.create_constraint("uniq_ts", "Event", "at").unwrap();
        let dt_utc = Value::DateTime {
            micros_utc: 1_000_000,
            offset_seconds: 0,
        };
        let dt_est = Value::DateTime {
            micros_utc: 1_000_000,
            offset_seconds: -18_000,
        };
        db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("Event", "e1", vec![("at".into(), dt_utc)]);
        })
        .unwrap();
        let err = db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("Event", "e2", vec![("at".into(), dt_est)]);
        });
        assert!(
            matches!(err, Err(GraphError::ConstraintViolation { ref constraint, .. }) if constraint == "uniq_ts"),
            "{err:?}"
        );
        // A different instant is a different value.
        db.insert_node(
            "Event",
            "e3",
            vec![(
                "at".into(),
                Value::DateTime {
                    micros_utc: 1_000_001,
                    offset_seconds: -18_000,
                },
            )],
        )
        .unwrap();
    }

    #[test]
    fn test_binary_size_limit() {
        // Review Focus #2.
        let (dir, mut db) = open_temp("binary");
        let ok_val = Value::Binary(vec![0u8; BINARY_MAX_BYTES]);
        db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("X", "k1", vec![("data".into(), ok_val)]);
        })
        .unwrap();
        let before = wal_bytes(&dir);
        let big_val = Value::Binary(vec![0u8; BINARY_MAX_BYTES + 1]);
        let err = db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("X", "k2", vec![("data".into(), big_val.clone())]);
        });
        match err {
            Err(GraphError::ValueTooLarge { field, size, limit }) => {
                assert_eq!(field, "data");
                assert_eq!(size, BINARY_MAX_BYTES + 1);
                assert_eq!(limit, BINARY_MAX_BYTES);
            }
            other => panic!("expected ValueTooLarge, got {other:?}"),
        }
        // set_prop, and a blob nested in a list, are held to the same limit.
        assert!(matches!(
            db.set_prop("k1", "data", big_val.clone()),
            Err(GraphError::ValueTooLarge { .. })
        ));
        assert!(matches!(
            db.set_prop("k1", "blobs", Value::List(vec![big_val])),
            Err(GraphError::ValueTooLarge { ref field, .. }) if field == "blobs"
        ));
        assert_eq!(wal_bytes(&dir), before, "a refused write appended");
        assert!(db.node_info("k2").is_none());
    }

    /// Differential probe: the same writes give the same answers whether they
    /// are separate commits or one batch, on a never-closed handle or a
    /// reopened one, with or without a snapshot in between.
    #[test]
    fn test_constraint_behavior_across_axes() {
        #[derive(Debug, Clone, Copy)]
        enum Axis {
            Live,
            Reopened,
            Snapshotted,
            SnapshotKeepWal,
        }
        let mut seen = Vec::new();
        for axis in [
            Axis::Live,
            Axis::Reopened,
            Axis::Snapshotted,
            Axis::SnapshotKeepWal,
        ] {
            for batched in [false, true] {
                let (dir, mut db) = open_temp("axes");
                db.create_constraint("uniq_email", "User", "email").unwrap();
                if batched {
                    db.write_batch(|b: &mut Batch<'_>| {
                        b.insert_node("User", "alice", email("a@x"));
                        b.insert_node("User", "carol", email("c@x"));
                        b.insert_node("Admin", "root", email("a@x"));
                    })
                    .unwrap();
                } else {
                    db.insert_node("User", "alice", email("a@x")).unwrap();
                    db.insert_node("User", "carol", email("c@x")).unwrap();
                    // Another label is another constraint's business.
                    db.insert_node("Admin", "root", email("a@x")).unwrap();
                }
                match axis {
                    Axis::Live => {}
                    Axis::Reopened => {
                        drop(db);
                        db = GraphDb::open(&dir).unwrap();
                    }
                    Axis::Snapshotted => {
                        db.snapshot().unwrap();
                        drop(db);
                        db = GraphDb::open(&dir).unwrap();
                    }
                    Axis::SnapshotKeepWal => {
                        db.snapshot_with(SnapshotOptions {
                            keep_wal: true,
                            ..Default::default()
                        })
                        .unwrap();
                        drop(db);
                        db = GraphDb::open(&dir).unwrap();
                    }
                }
                assert_eq!(db.constraints().len(), 1, "{axis:?} batched={batched}");
                assert!(db.is_index_enabled("User", "email"));
                let before = wal_bytes(&dir);
                assert_violation(db.insert_node("User", "bob", email("a@x")));
                assert_violation(db.set_prop("carol", "email", Value::Str("a@x".into())));
                assert_eq!(wal_bytes(&dir), before, "{axis:?} batched={batched}");
                db.insert_node("User", "bob", email("b@x")).unwrap();
                let mut state: Vec<_> = ["alice", "bob", "carol", "root"]
                    .iter()
                    .map(|k| {
                        db.node_info(k)
                            .map(|n| (n.label, n.props.get("email").cloned()))
                    })
                    .collect();
                state.push(None);
                seen.push((format!("{axis:?} batched={batched}"), state));
            }
        }
        let first = &seen[0].1;
        for (name, state) in &seen {
            assert_eq!(state, first, "{name} disagrees with {}", seen[0].0);
        }
    }

    // ── One test per write path ──────────────────────────────────────────────

    #[test]
    fn insert_node_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-insert-node");
        let before = wal_bytes(&dir);
        assert_violation(db.insert_node("User", "bob", email("a@x")));
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn set_prop_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-set-prop");
        db.insert_node("User", "bob", email("b@x")).unwrap();
        let before = wal_bytes(&dir);
        assert_violation(db.set_prop("bob", "email", Value::Str("a@x".into())));
        assert_eq!(wal_bytes(&dir), before);
        assert_eq!(
            db.node_info("bob").unwrap().props.get("email"),
            Some(&Value::Str("b@x".into()))
        );
    }

    #[test]
    fn set_props_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-set-props");
        db.insert_node("User", "bob", email("b@x")).unwrap();
        let before = wal_bytes(&dir);
        assert_violation(db.set_props(
            "bob",
            vec![
                ("name".into(), Value::Str("Bob".into())),
                ("email".into(), Value::Str("a@x".into())),
            ],
        ));
        assert_eq!(wal_bytes(&dir), before);
        assert!(!db.node_info("bob").unwrap().props.contains_key("name"));
    }

    #[test]
    fn upsert_refuses_a_duplicate() {
        // insert_node_on_conflict(Replace) is the upsert: it inserts a new key
        // and rewrites an existing one. Both shapes are checked.
        for existing in [false, true] {
            let (dir, mut db) = constrained("p-upsert");
            if existing {
                db.insert_node("User", "bob", email("b@x")).unwrap();
            }
            let before = wal_bytes(&dir);
            assert_violation(db.write_batch(|b: &mut Batch<'_>| {
                b.insert_node_on_conflict("User", "bob", email("a@x"), OnConflict::Replace);
            }));
            assert_eq!(wal_bytes(&dir), before, "existing={existing}");
        }
        // Re-supplying a node's own value is not a conflict with itself.
        let (_dir, mut db) = constrained("p-upsert-self");
        db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node_on_conflict("User", "alice", email("a@x"), OnConflict::Replace);
        })
        .unwrap();
    }

    #[test]
    fn write_batch_refuses_the_whole_batch() {
        let (dir, mut db) = constrained("p-write-batch");
        let before = wal_bytes(&dir);
        // The duplicate is the last op: the plain ops before it must not land.
        assert_violation(db.write_batch(|b: &mut Batch<'_>| {
            b.insert_node("User", "carol", email("c@x"));
            b.insert_edge("KNOWS", "alice", "carol");
            b.insert_node("User", "bob", vec![]);
            b.set_prop("bob", "email", Value::Str("a@x".into()));
        }));
        assert!(db.node_info("carol").is_none());
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn ingest_rows_refuse_a_duplicate() {
        let (dir, mut db) = constrained("p-ingest");
        let before = wal_bytes(&dir);
        let rows = vec![
            BTreeMap::from([
                ("id".to_string(), Value::Str("carol".into())),
                ("email".to_string(), Value::Str("c@x".into())),
            ]),
            BTreeMap::from([
                ("id".to_string(), Value::Str("bob".into())),
                ("email".to_string(), Value::Str("a@x".into())),
            ]),
        ];
        assert_violation(db.ingest("User", rows, &IngestOptions::default()));
        assert!(db.node_info("carol").is_none());
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn ingest_json_refuses_a_duplicate_inside_the_payload() {
        let (dir, mut db) = constrained("p-ingest-json");
        let before = wal_bytes(&dir);
        let json = r#"[{"id": "bob", "email": "z@x"}, {"id": "carol", "email": "z@x"}]"#;
        assert_violation(db.ingest_json("User", json, &IngestOptions::default()));
        assert!(db.node_info("carol").is_none());
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn cypher_create_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-cypher-create");
        let before = wal_bytes(&dir);
        assert_violation(db.query_write(
            "CREATE (n:User {id: 'bob', email: 'a@x'})",
            &BTreeMap::new(),
        ));
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn cypher_set_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-cypher-set");
        db.insert_node("User", "bob", email("b@x")).unwrap();
        let before = wal_bytes(&dir);
        let params = BTreeMap::from([("e".to_string(), Value::Str("a@x".into()))]);
        assert_violation(db.query_write(
            "MATCH (n:User) WHERE n.email = 'b@x' SET n.email = $e",
            &params,
        ));
        assert_eq!(wal_bytes(&dir), before);
        assert_eq!(
            db.node_info("bob").unwrap().props.get("email"),
            Some(&Value::Str("b@x".into()))
        );
    }

    #[test]
    fn cypher_merge_on_create_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-cypher-merge");
        let before = wal_bytes(&dir);
        assert_violation(db.query_write(
            "MERGE (n:User {id: 'bob'}) ON CREATE SET n.email = 'a@x'",
            &BTreeMap::new(),
        ));
        assert_nothing_written(&dir, db, &before);
    }

    #[test]
    fn cypher_merge_matching_the_holder_is_not_a_conflict() {
        // The match leg finds the holder, so ON CREATE SET does not run, and
        // ON MATCH SET writing the holder's own value is no clash.
        let (_dir, mut db) = open_temp("p-cypher-merge-match");
        db.create_constraint("uniq_email", "User", "email").unwrap();
        let mut props = email("a@x");
        props.push(("id".into(), Value::Str("alice".into())));
        db.insert_node("User", "alice", props).unwrap();
        db.query_write(
            "MERGE (n:User {id: 'alice'}) ON CREATE SET n.email = 'a@x' \
             ON MATCH SET n.email = 'a@x'",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(db.node_count(), 1);
    }

    #[test]
    fn shared_db_submit_batch_refuses_a_duplicate() {
        let dir = tmp("p-shared");
        let db = SharedDb::open(&dir).unwrap();
        {
            let mut w = db.write();
            w.unlock_v11_format_for_test();
            w.create_constraint("uniq_email", "User", "email").unwrap();
            w.insert_node("User", "alice", email("a@x")).unwrap();
        }
        let before = wal_bytes(&dir);
        assert_violation(db.submit_batch(vec![BatchOp::InsertNode {
            label: "User".into(),
            key: "bob".into(),
            props: email("a@x"),
        }]));
        assert_eq!(wal_bytes(&dir), before);
        assert!(db.read().node_info("bob").is_none());
    }

    #[test]
    fn a_role_scoped_write_gets_the_same_error_for_a_hidden_holder() {
        // Spec §4.6: the check is global, and its error does not say whether
        // the node holding the value is one the writer can see.
        let (dir, mut db) = open_temp("p-hidden");
        db.create_constraint("uniq_email", "User", "email").unwrap();
        let mut hidden = email("hidden@x");
        hidden.push(("tier".into(), Value::Str("private".into())));
        db.insert_node("User", "alice", hidden).unwrap();
        let mut visible = email("seen@x");
        visible.push(("tier".into(), Value::Str("public".into())));
        db.insert_node("User", "carol", visible).unwrap();
        db.apply_schema(&mushroomdb::schema::Schema {
            roles: vec![RoleDef {
                name: "writer".into(),
                keys: vec![],
                labels: vec!["User".into()],
                visible_where: Some(PropPredicate {
                    field: "tier".into(),
                    eq: Some(Value::Str("public".into())),
                    in_: None,
                }),
                namespaces: None,
                write: Some(WriteScope {
                    create_labels: vec!["User".into()],
                    ..Default::default()
                }),
            }],
            ..Default::default()
        })
        .unwrap();
        let roles = db.roles();
        let scope = roles[0].write.clone().unwrap();
        let authz = WriteAuthz {
            role: "writer".into(),
            scope,
            mask: db.mask_for_role("writer").unwrap(),
        };
        let before = wal_bytes(&dir);
        let insert = |value: &str| {
            let mut props = email(value);
            props.push(("tier".into(), Value::Str("public".into())));
            vec![BatchOp::InsertNode {
                label: "User".into(),
                key: "bob".into(),
                props,
            }]
        };
        let against_hidden = db
            .write_batch_authz(Some(&authz), insert("hidden@x"))
            .unwrap_err();
        let against_visible = db
            .write_batch_authz(Some(&authz), insert("seen@x"))
            .unwrap_err();
        assert!(matches!(
            against_hidden,
            GraphError::ConstraintViolation { .. }
        ));
        assert_eq!(against_hidden.to_string(), against_visible.to_string());
        let text = against_hidden.to_string();
        for leak in ["alice", "carol", "User", "private", "hidden@x"] {
            assert!(!text.contains(leak), "{text:?} names {leak:?}");
        }
        assert_eq!(wal_bytes(&dir), before);
    }

    #[test]
    fn write_batch_cas_refuses_a_duplicate() {
        let (dir, mut db) = constrained("p-cas");
        let before = wal_bytes(&dir);
        assert_violation(db.write_batch_cas(
            vec![Precondition::NodeAbsent { key: "bob".into() }],
            vec![BatchOp::InsertNode {
                label: "User".into(),
                key: "bob".into(),
                props: email("a@x"),
            }],
        ));
        assert_nothing_written(&dir, db, &before);
    }

    // ── Views write their property after the check, so the two never meet ───

    fn deg_view() -> ViewDef {
        ViewDef {
            name: "deg".into(),
            label: "User".into(),
            view_prop: "deg".into(),
            source: ViewSource::Degree {
                edge_type: "KNOWS".into(),
                direction: Direction::Out,
            },
        }
    }

    #[test]
    fn create_constraint_refuses_a_view_owned_property() {
        let (dir, mut db) = open_temp("view-then-constraint");
        db.create_view(deg_view()).unwrap();
        let before = wal_bytes(&dir);
        match db.create_constraint("uniq_deg", "User", "deg") {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "property 'deg' on label 'User' is owned by view 'deg'; a unique constraint \
                 cannot cover it"
            ),
            other => panic!("expected a QueryError, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert!(db.constraints().is_empty());
        // Another label's `deg` is not the view's.
        db.create_constraint("uniq_admin_deg", "Admin", "deg")
            .unwrap();
    }

    #[test]
    fn create_view_refuses_a_constrained_property() {
        let (dir, mut db) = open_temp("constraint-then-view");
        db.create_constraint("uniq_deg", "User", "deg").unwrap();
        let before = wal_bytes(&dir);
        match db.create_view(deg_view()) {
            Err(GraphError::QueryError { detail }) => assert_eq!(
                detail,
                "property 'deg' on label 'User' is declared unique by constraint 'uniq_deg'; a \
                 view cannot own it"
            ),
            other => panic!("expected a QueryError, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert!(db.views().is_empty());
    }

    /// The review's scenario: a Degree view on User and a constraint on its
    /// property. Each new User got `deg = 0` inside `apply`, after the check,
    /// so two Users held the same constrained value with no error. In either
    /// declaration order the second declaration is now refused, so whatever
    /// constraint exists holds over the nodes written afterwards.
    #[test]
    fn a_view_and_a_constraint_cannot_combine_into_duplicates() {
        for view_first in [true, false] {
            let (_dir, mut db) = open_temp("view-dups");
            if view_first {
                db.create_view(deg_view()).unwrap();
                assert!(db.create_constraint("uniq_deg", "User", "deg").is_err());
            } else {
                db.create_constraint("uniq_deg", "User", "deg").unwrap();
                assert!(db.create_view(deg_view()).is_err());
            }
            db.insert_node("User", "u1", vec![]).unwrap();
            db.insert_node("User", "u2", vec![]).unwrap();
            let degs: Vec<_> = ["u1", "u2"]
                .iter()
                .map(|k| db.node_info(k).unwrap().props.get("deg").cloned())
                .collect();
            let constrained = db
                .constraints()
                .iter()
                .any(|c| c.label == "User" && c.field == "deg");
            if constrained {
                assert!(
                    degs.iter().all(Option::is_none),
                    "view_first={view_first}: no view may write the constrained property"
                );
            } else {
                assert_eq!(degs, vec![Some(Value::Int(0)), Some(Value::Int(0))]);
            }
        }
    }

    // ── Replay never checks ─────────────────────────────────────────────────

    #[test]
    fn a_wal_with_duplicates_before_the_constraint_reopens() {
        // House rule: a check for new writes never runs on replay. Old records
        // may violate a constraint declared after them, and replay must not
        // fail on them.
        let (dir, mut db) = open_temp("replay-dups");
        db.insert_node("User", "alice", email("a@x")).unwrap();
        db.insert_node("User", "bob", email("a@x")).unwrap();
        // No pre-scan in TY-5 (TY-6 adds it), so this declares over them.
        db.create_constraint("uniq_email", "User", "email").unwrap();
        db.insert_node("User", "carol", email("c@x")).unwrap();
        let live_count = db.node_count();
        drop(db);

        let mut reopened = GraphDb::open(&dir).expect("replay must never fail");
        assert_eq!(reopened.node_count(), live_count);
        assert_eq!(reopened.constraints().len(), 1);
        assert_eq!(
            reopened.node_info("bob").unwrap().props.get("email"),
            Some(&Value::Str("a@x".into()))
        );
        // New writes are still held to it.
        assert_violation(reopened.insert_node("User", "dave", email("c@x")));
        assert_violation(reopened.insert_node("User", "dave", email("a@x")));
        drop(reopened);

        // As-of reads replay the same records and are not refused either.
        for commit in 0..4 {
            GraphDb::open_at(&dir, commit).expect("open_at must never fail");
        }
        let before = GraphDb::open_at(&dir, 1).unwrap();
        assert!(before.constraints().is_empty(), "declared at commit 2");
        assert_eq!(GraphDb::open_at(&dir, 2).unwrap().constraints().len(), 1);
    }

    // ── What the frame leaves behind is what is judged ──────────────────────

    #[test]
    fn a_frame_that_frees_a_value_may_take_it() {
        let (_dir, mut db) = constrained("frees");
        db.insert_node("User", "bob", email("b@x")).unwrap();
        // Swap two values in one commit.
        db.write_batch(|b: &mut Batch<'_>| {
            b.set_prop("alice", "email", Value::Str("b@x".into()));
            b.set_prop("bob", "email", Value::Str("a@x".into()));
        })
        .unwrap();
        // Delete the holder and give its value to a new node.
        db.write_batch(|b: &mut Batch<'_>| {
            b.delete_node("alice");
            b.insert_node("User", "carol", email("b@x"));
        })
        .unwrap();
        // Remove a value and hand it on.
        db.write_batch(|b: &mut Batch<'_>| {
            b.remove_prop("carol", "email");
            b.set_prop("bob", "email", Value::Str("b@x".into()));
        })
        .unwrap();
        // Writing a node's own value again is no conflict.
        db.set_prop("bob", "email", Value::Str("b@x".into()))
            .unwrap();
        // A rename keeps the node, and so its value.
        db.write_batch(|b: &mut Batch<'_>| {
            b.rename_node("bob", "robert");
            b.set_prop("robert", "email", Value::Str("b@x".into()));
        })
        .unwrap();
        assert_violation(db.insert_node("User", "eve", email("b@x")));
        // Deleting and re-inserting a key in one frame is a new node, which
        // still may not take a value another node keeps.
        db.insert_node("User", "frank", email("f@x")).unwrap();
        assert_violation(db.write_batch(|b: &mut Batch<'_>| {
            b.delete_node("frank");
            b.insert_node("User", "frank", email("b@x"));
        }));
    }

    #[test]
    fn values_without_an_index_key_and_other_labels_never_conflict() {
        let (_dir, mut db) = constrained("no-key");
        let list = Value::List(vec![Value::Int(1)]);
        db.insert_node("User", "l1", vec![("email".into(), list.clone())])
            .unwrap();
        db.insert_node("User", "l2", vec![("email".into(), list)])
            .unwrap();
        db.insert_node("Admin", "root", email("a@x")).unwrap();
        // An Int and a Str are different keys.
        db.insert_node("User", "n", vec![("email".into(), Value::Int(1))])
            .unwrap();
        db.insert_node("User", "s", email("1")).unwrap();
    }

    #[test]
    fn a_refused_write_leaves_no_interned_name_behind() {
        // The refused insert names a field nothing else uses. If it were
        // interned without its `Intern` record reaching the WAL, the next
        // successful write would log ids that replay does not reproduce.
        let (dir, mut db) = constrained("intern-rollback");
        let mut props = email("a@x");
        props.push(("fresh_field".into(), Value::Int(1)));
        assert_violation(db.insert_node("User", "bob", props));
        db.insert_node(
            "Later",
            "carol",
            vec![("fresh_field".into(), Value::Int(7))],
        )
        .unwrap();
        let live = db.node_info("carol").unwrap();
        drop(db);
        let reopened = GraphDb::open(&dir).unwrap();
        assert_eq!(reopened.node_info("carol"), Some(live));
    }

    // ── Declaration lifecycle ────────────────────────────────────────────────

    #[test]
    fn dropping_a_constraint_keeps_its_index_and_stops_the_check() {
        let (dir, mut db) = constrained("drop");
        assert!(matches!(
            db.disable_index("User", "email"),
            Err(GraphError::QueryError { ref detail }) if detail.contains("uniq_email")
        ));
        assert!(matches!(
            db.create_constraint("uniq_email", "User", "email"),
            Err(GraphError::QueryError { .. })
        ));
        db.drop_constraint("uniq_email").unwrap();
        assert!(db.constraints().is_empty());
        assert!(db.is_index_enabled("User", "email"), "the index stays");
        db.insert_node("User", "bob", email("a@x")).unwrap();
        assert!(matches!(
            db.drop_constraint("uniq_email"),
            Err(GraphError::QueryError { .. })
        ));
        drop(db);
        let mut reopened = GraphDb::open(&dir).unwrap();
        assert!(reopened.constraints().is_empty());
        assert!(reopened.is_index_enabled("User", "email"));
        reopened.disable_index("User", "email").unwrap();
    }

    #[test]
    fn a_locked_handle_enforces_a_replayed_constraint_and_refuses_to_drop_it() {
        // A store whose WAL already holds a constraint, opened by a handle
        // without the hook: the constraint is enforced, and withdrawing it —
        // another V11 record — is refused at the same choke point.
        let (dir, db) = constrained("locked-reopen");
        drop(db);
        let mut locked = GraphDb::open(&dir).unwrap();
        let before = wal_bytes(&dir);
        assert_violation(locked.insert_node("User", "bob", email("a@x")));
        match locked.drop_constraint("uniq_email") {
            Err(GraphError::QueryError { detail }) => assert_eq!(detail, CONSTRAINT_REFUSAL),
            other => panic!("expected the V11 record refusal, got {other:?}"),
        }
        assert_eq!(wal_bytes(&dir), before);
        assert_eq!(locked.constraints().len(), 1);
    }

    #[test]
    fn a_truncating_snapshot_re_emits_the_declaration() {
        let (dir, mut db) = constrained("snapshot-truncate");
        db.snapshot().unwrap();
        db.snapshot().unwrap();
        drop(db);
        let mut reopened = GraphDb::open(&dir).unwrap();
        assert_eq!(reopened.constraints().len(), 1);
        assert_violation(reopened.insert_node("User", "bob", email("a@x")));
    }
    // ── Cypher REMOVE and SET maps (QL-05, QL-06) ────────────────────────────

    fn cypher(db: &mut GraphDb, q: &str) -> Result<mushroomdb::ResultSet, GraphError> {
        db.query_write(q, &BTreeMap::new())
    }

    #[test]
    fn cypher_remove_of_a_unique_property_frees_the_value_for_another_node() {
        let (dir, mut db) = constrained("cypher-remove");
        cypher(
            &mut db,
            "MATCH (n:User) WHERE n.id = 'alice' REMOVE n.email",
        )
        .unwrap();
        assert_eq!(db.get_prop("alice", "email"), None);
        cypher(&mut db, "CREATE (n:User {id: 'bob', email: 'a@x'})").unwrap();
        assert_eq!(db.get_prop("bob", "email"), Some(Value::Str("a@x".into())));
        // Alice can no longer take it back.
        assert_violation(cypher(
            &mut db,
            "MATCH (n:User) WHERE n.id = 'alice' SET n.email = 'a@x'",
        ));
        drop(db);
        let reopened = GraphDb::open(&dir).unwrap();
        assert_eq!(reopened.get_prop("alice", "email"), None);
        assert_eq!(
            reopened.get_prop("bob", "email"),
            Some(Value::Str("a@x".into()))
        );
    }

    #[test]
    fn cypher_set_maps_meet_the_unique_constraint_and_write_nothing_when_refused() {
        let (dir, mut db) = constrained("cypher-set-map");
        cypher(&mut db, "CREATE (n:User {id: 'bob', email: 'b@x', age: 3})").unwrap();
        let before = wal_bytes(&dir);
        for q in [
            "MATCH (n:User) WHERE n.id = 'bob' SET n = {email: 'a@x'}",
            "MATCH (n:User) WHERE n.id = 'bob' SET n += {email: 'a@x', age: 4}",
        ] {
            assert_violation(cypher(&mut db, q));
            assert_eq!(wal_bytes(&dir), before, "{q}: a refused write appended");
            assert_eq!(db.get_prop("bob", "email"), Some(Value::Str("b@x".into())));
            assert_eq!(db.get_prop("bob", "age"), Some(Value::Int(3)), "{q}");
        }
        // A replace that drops the property frees its value in the same store.
        cypher(
            &mut db,
            "MATCH (n:User) WHERE n.id = 'alice' SET n = {name: 'A'}",
        )
        .unwrap();
        assert_eq!(db.get_prop("alice", "email"), None);
        cypher(
            &mut db,
            "MATCH (n:User) WHERE n.id = 'bob' SET n += {email: 'a@x'}",
        )
        .unwrap();
        assert_eq!(db.get_prop("bob", "email"), Some(Value::Str("a@x".into())));
        // `+= {email: null}` frees it again.
        cypher(
            &mut db,
            "MATCH (n:User) WHERE n.id = 'bob' SET n += {email: null}",
        )
        .unwrap();
        cypher(&mut db, "CREATE (n:User {id: 'carol', email: 'a@x'})").unwrap();
    }
}
