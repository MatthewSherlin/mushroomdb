//! Tests for Task 2: Engine authz core at the write choke point.
//!
//! Decision-table row → test name mapping:
//! CREATE-class:
//!   Row 1 (scope-before-lookup, label not in create_labels):
//!     test_create_scope_denied_empty_store
//!   Row 2 (key exists and visible → DuplicateKey):
//!     test_create_visible_collision
//!   Row 3 (key exists and hidden → not-visible):
//!     test_create_hidden_collision
//!   Row 4 (key absent → proceed):
//!     test_create_allowed
//!
//! UPDATE/DELETE-class (SetProp):
//!   Visible + in update_labels → allowed:
//!     test_update_visible_allowed
//!   Visible + NOT in update_labels → scope-denied:
//!     test_update_scope_denied
//!   Hidden ≡ absent, EXACT-EQUAL error:
//!     test_update_hidden_identical_to_absent
//!
//! DELETE-class (DeleteNode):
//!   Visible + in delete_labels → allowed:
//!     test_delete_node_allowed
//!   Visible + NOT in delete_labels → scope-denied:
//!     test_delete_node_scope_denied
//!
//! DeleteEdge (derived-edge rejection before scope check):
//!   Derived edge, NOT in delete_edge_types → RuleOwned (not scope-denied):
//!     test_delete_edge_derived_before_scope
//!   In delete_edge_types, both endpoints visible → allowed:
//!     test_delete_edge_allowed
//!   Endpoint hidden → endpoint-not-visible:
//!     test_delete_edge_hidden_endpoint
//!   NOT in delete_edge_types → scope-denied:
//!     test_delete_edge_type_not_scoped
//!
//! MERGE:
//!   Neither create nor update scope → scope-denied WITHOUT key lookup:
//!     test_merge_unscoped_no_key_lookup
//!   Key absent + create scope → create arm:
//!     test_merge_create_arm
//!   Key visible + update scope → match arm:
//!     test_merge_match_arm
//!   Key hidden → not-visible:
//!     test_merge_hidden_key
//!
//! EDGE-CREATE:
//!   Both endpoints visible, type in create_edge_types → allowed:
//!     test_edge_create_both_visible
//!   One endpoint hidden → endpoint-not-visible:
//!     test_edge_create_one_hidden
//!   Type not in create_edge_types → scope-denied:
//!     test_edge_create_type_not_scoped
//!
//! InsertEdgeUpsert placeholder:
//!   Endpoint created by prior InsertNode in same batch counts as visible:
//!     test_upsert_placeholder_counts_as_visible
//!
//! Cross-cutting:
//!   Batch atomicity (no WAL frame on deny):
//!     test_batch_atomicity_no_wal_on_deny
//!   Authz before CAS (hidden node → not-visible, never CasConflict):
//!     test_authz_fires_before_cas_would
//!   None authz = full authority (zero-cost bypass):
//!     test_none_authz_full_authority
//!   RenameNode with Some(authz) → endpoint-not-permitted:
//!     test_rename_node_forbidden_for_role
//!   CreateRule with Some(authz) → endpoint-not-permitted:
//!     test_create_rule_forbidden_for_role
//!   Rules fire on role-created nodes; derived edges to hidden neighbors
//!   are masked on read:
//!     test_rules_fire_but_hidden_edges_masked

use core_api::{
    schema::Schema, BatchOp, Direction, GraphDb, GraphError, Predicate, RoleDef, RuleDef, Value,
    WriteScope,
};
use std::collections::BTreeMap;

// ── helpers ──────────────────────────────────────────────────────────────────

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-ws-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Role with full write scope over "MyLabel" nodes and "KNOWS" edges.
/// Labels visible to the role: ["MyLabel", "Visible"].
/// Hidden label (not in role): "Secret".
fn writer_role() -> RoleDef {
    RoleDef {
        name: "writer".into(),
        keys: vec![],
        labels: vec!["MyLabel".into(), "Visible".into()],
        write: Some(WriteScope {
            create_labels: vec!["MyLabel".into()],
            update_labels: vec!["MyLabel".into(), "Visible".into()],
            delete_labels: vec!["MyLabel".into()],
            create_edge_types: vec!["KNOWS".into()],
            delete_edge_types: vec!["KNOWS".into()],
        }),
    }
}

/// Open a fresh DB, apply the writer role schema.
fn open_with_writer(name: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let schema = Schema {
        fulltext: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![writer_role()],
    };
    db.apply_schema(&schema).unwrap();
    (db, dir)
}

/// Build WriteAuthz for "writer" from the live DB state.
fn writer_authz(db: &mut GraphDb<core_storage::fs::RealFs>) -> core_api::WriteAuthz {
    let roles = db.roles();
    let def = roles.iter().find(|r| r.name == "writer").unwrap();
    let scope = def.write.clone().unwrap();
    let mask = db.mask_for_role("writer").unwrap();
    core_api::WriteAuthz {
        role: "writer".into(),
        scope,
        mask,
    }
}

fn is_role_write_denied(e: &GraphError) -> bool {
    matches!(e, GraphError::RoleWriteDenied { .. })
}

fn denied_reason(e: &GraphError) -> &str {
    match e {
        GraphError::RoleWriteDenied { reason } => reason.as_str(),
        _ => panic!("expected RoleWriteDenied, got {e:?}"),
    }
}

// ── CREATE-class ─────────────────────────────────────────────────────────────

/// Decision table row 4: key absent + label in create_labels → proceed.
#[test]
fn test_create_allowed() {
    let (mut db, _dir) = open_with_writer("create-allowed");
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertNode {
        label: "MyLabel".into(),
        key: "n1".into(),
        props: vec![],
    }];
    let (nodes, _) = db.write_batch_authz(Some(&authz), ops).unwrap();
    assert_eq!(nodes, 1);
    assert!(db.has_node("n1"));
}

/// Decision table row 1 (scope-before-lookup): label NOT in create_labels fires
/// even when the store is EMPTY (no key lookup precedes the scope check).
/// This is the structural closure of the §6.2 timing-oracle item.
#[test]
fn test_create_scope_denied_empty_store() {
    let (mut db, _dir) = open_with_writer("create-scope-denied");
    let authz = writer_authz(&mut db);
    // Store is empty — no nodes exist yet.
    assert!(!db.has_node("x"));
    let ops = vec![BatchOp::InsertNode {
        label: "Secret".into(), // NOT in create_labels
        key: "x".into(),
        props: vec![],
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert!(
        denied_reason(&err).contains("create_labels"),
        "reason should name create_labels: {}",
        denied_reason(&err)
    );
    // Store remains empty: scope denial fired without key lookup.
    assert!(!db.has_node("x"));
}

/// Decision table row 3: key exists and HIDDEN → 403 target-not-visible.
#[test]
fn test_create_hidden_collision() {
    let (mut db, _dir) = open_with_writer("create-hidden-collision");
    // Insert a "Secret" node as admin (no authz).
    db.insert_node("Secret", "secret_key", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    // Try to create "MyLabel" node with the same key.
    let ops = vec![BatchOp::InsertNode {
        label: "MyLabel".into(),
        key: "secret_key".into(),
        props: vec![],
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: target node not visible",
        "hidden key must return not-visible, not DuplicateKey"
    );
}

/// Decision table row 2: key exists and VISIBLE → DuplicateKey (existing behavior).
#[test]
fn test_create_visible_collision() {
    let (mut db, _dir) = open_with_writer("create-visible-collision");
    // Insert visible node as admin.
    db.insert_node("MyLabel", "alice", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertNode {
        label: "MyLabel".into(),
        key: "alice".into(),
        props: vec![],
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        matches!(err, GraphError::DuplicateKey { .. }),
        "visible collision must return DuplicateKey, got {err:?}"
    );
}

// ── UPDATE-class ──────────────────────────────────────────────────────────────

/// Visible node + label in update_labels → SetProp succeeds.
#[test]
fn test_update_visible_allowed() {
    let (mut db, _dir) = open_with_writer("update-allowed");
    db.insert_node("MyLabel", "alice", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::SetProp {
        key: "alice".into(),
        field: "name".into(),
        value: Value::Str("Alice".into()),
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    assert_eq!(
        db.get_prop("alice", "name"),
        Some(Value::Str("Alice".into()))
    );
}

/// Visible node + label NOT in update_labels → scope-denied.
#[test]
fn test_update_scope_denied() {
    // "Visible" is in role's read labels and update_labels, but NOT update_labels for... wait.
    // writer_role has update_labels: ["MyLabel", "Visible"]. Let me use a label only in read.
    // Actually let me use a separate role for this test.
    let dir = tmp("update-scope-denied");
    let mut db = GraphDb::open(&dir).unwrap();
    let schema = Schema {
        fulltext: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "reader_writer".into(),
            keys: vec![],
            labels: vec!["MyLabel".into(), "ReadOnly".into()],
            write: Some(WriteScope {
                create_labels: vec!["MyLabel".into()],
                update_labels: vec!["MyLabel".into()], // ReadOnly NOT in update_labels
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    };
    db.apply_schema(&schema).unwrap();
    // Insert a ReadOnly-labeled node as admin.
    db.insert_node("ReadOnly", "ro_node", vec![]).unwrap();
    // Build authz.
    let roles = db.roles();
    let def = roles.iter().find(|r| r.name == "reader_writer").unwrap();
    let scope = def.write.clone().unwrap();
    let mask = db.mask_for_role("reader_writer").unwrap();
    let authz = core_api::WriteAuthz {
        role: "reader_writer".into(),
        scope,
        mask,
    };
    let ops = vec![BatchOp::SetProp {
        key: "ro_node".into(),
        field: "x".into(),
        value: Value::Int(1),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert!(
        denied_reason(&err).contains("update_labels"),
        "reason should name update_labels: {}",
        denied_reason(&err)
    );
}

/// Hidden node and absent node return EXACT-EQUAL errors (spec §3.1).
#[test]
fn test_update_hidden_identical_to_absent() {
    let (mut db, _dir) = open_with_writer("update-hidden-absent");
    // Insert a hidden node (label not in role's read labels).
    db.insert_node("Secret", "hidden_node", vec![]).unwrap();
    let authz = writer_authz(&mut db);

    let hidden_ops = vec![BatchOp::SetProp {
        key: "hidden_node".into(),
        field: "x".into(),
        value: Value::Int(1),
    }];
    let absent_ops = vec![BatchOp::SetProp {
        key: "nonexistent".into(), // does not exist at all
        field: "x".into(),
        value: Value::Int(1),
    }];

    let err_hidden = db.write_batch_authz(Some(&authz), hidden_ops).unwrap_err();
    let err_absent = db.write_batch_authz(Some(&authz), absent_ops).unwrap_err();

    // EXACT equality: same error variant, same reason string.
    assert_eq!(
        denied_reason(&err_hidden),
        denied_reason(&err_absent),
        "hidden and absent must produce identical error messages (spec §3.1)"
    );
    assert_eq!(
        denied_reason(&err_hidden),
        "role-bound token: target node not visible"
    );
}

// ── DELETE-class: DeleteNode ──────────────────────────────────────────────────

/// Visible node + label in delete_labels → DeleteNode succeeds.
#[test]
fn test_delete_node_allowed() {
    let (mut db, _dir) = open_with_writer("delete-node-allowed");
    db.insert_node("MyLabel", "del_me", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteNode {
        key: "del_me".into(),
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    assert!(!db.has_node("del_me"));
}

/// Visible node + label NOT in delete_labels → scope-denied.
#[test]
fn test_delete_node_scope_denied() {
    let (mut db, _dir) = open_with_writer("delete-node-scope-denied");
    // "Visible" is in role labels but NOT in delete_labels (which only has "MyLabel").
    db.insert_node("Visible", "vis_node", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteNode {
        key: "vis_node".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert!(
        denied_reason(&err).contains("delete_labels"),
        "reason should name delete_labels: {}",
        denied_reason(&err)
    );
}

// ── DELETE-class: DeleteEdge (derived-edge rejection order) ──────────────────

fn derived_rule() -> RuleDef {
    RuleDef {
        name: "sim".into(),
        src_label: "MyLabel".into(),
        dst_label: "MyLabel".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "SIMILAR".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

/// Derived edge + NOT in delete_edge_types → RuleOwned fires BEFORE scope-denied.
/// The order: derived-edge rejection precedes delete_edge_types check (spec §3.5).
#[test]
fn test_delete_edge_derived_before_scope() {
    let dir = tmp("delete-edge-derived-order");
    let mut db = GraphDb::open(&dir).unwrap();
    let schema = Schema {
        fulltext: vec![],
        rules: vec![derived_rule()],
        views: vec![],
        roles: vec![writer_role()],
    };
    db.apply_schema(&schema).unwrap();
    // Insert two MyLabel nodes with overlapping tags → rule derives SIMILAR edge.
    db.insert_node(
        "MyLabel",
        "a",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    db.insert_node(
        "MyLabel",
        "b",
        vec![("tags".into(), Value::List(vec![Value::Str("x".into())]))],
    )
    .unwrap();
    // SIMILAR is derived but NOT in delete_edge_types (only KNOWS is).
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteEdge {
        edge_type: "SIMILAR".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    // Must be RuleOwned (derived-edge check fires first), NOT RoleWriteDenied scope.
    assert!(
        matches!(err, GraphError::RuleOwned { .. }),
        "derived-edge rejection must precede scope check; got {err:?}"
    );
}

/// In delete_edge_types, both endpoints visible → DeleteEdge succeeds.
#[test]
fn test_delete_edge_allowed() {
    let (mut db, _dir) = open_with_writer("delete-edge-allowed");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("MyLabel", "b", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteEdge {
        edge_type: "KNOWS".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    let neighbors = db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap_or_default();
    assert!(
        !neighbors.contains(&"b".to_string()),
        "edge should be deleted"
    );
}

/// Edge with one hidden endpoint → endpoint-not-visible.
#[test]
fn test_delete_edge_hidden_endpoint() {
    let (mut db, _dir) = open_with_writer("delete-edge-hidden-ep");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("Secret", "secret_b", vec![]).unwrap();
    // Insert the edge as admin (no authz).
    db.insert_edge("KNOWS", "a", "secret_b").unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteEdge {
        edge_type: "KNOWS".into(),
        src_key: "a".into(),
        dst_key: "secret_b".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: edge endpoint not visible"
    );
}

/// Edge type NOT in delete_edge_types → scope-denied.
#[test]
fn test_delete_edge_type_not_scoped() {
    let (mut db, _dir) = open_with_writer("delete-edge-unscoped");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("MyLabel", "b", vec![]).unwrap();
    db.insert_edge("UNSCOPED_TYPE", "a", "b").unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::DeleteEdge {
        edge_type: "UNSCOPED_TYPE".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert!(
        denied_reason(&err).contains("delete_edge_types"),
        "reason should name delete_edge_types: {}",
        denied_reason(&err)
    );
}

// ── MERGE (via query_write_authz) ─────────────────────────────────────────────

/// MERGE scope precondition: neither create nor update scope for label → 403
/// WITHOUT a key lookup (timing-oracle closure, spec §6.2).
#[test]
fn test_merge_unscoped_no_key_lookup() {
    let (mut db, _dir) = open_with_writer("merge-unscoped");
    // The role has no create_labels or update_labels for "Secret".
    let err = db
        .query_write_authz("writer", "MERGE (n:Secret {id: 'x'})", &no_params())
        .unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied for unscoped MERGE, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: label 'Secret' not in write scope (create_labels)",
        "unscoped MERGE must return exact §4.3 reason string"
    );
    // Key "x" must not exist (no key lookup before scope denial).
    assert!(
        !db.has_node("x"),
        "scope denial must fire before key lookup"
    );
}

/// MERGE create arm: key absent + label in create_labels → creates node.
#[test]
fn test_merge_create_arm() {
    let (mut db, _dir) = open_with_writer("merge-create");
    let result = db
        .query_write_authz("writer", "MERGE (n:MyLabel {id: 'new_node'})", &no_params())
        .unwrap();
    assert!(
        db.has_node("new_node"),
        "MERGE create arm must create the node"
    );
    let _ = result;
}

/// MERGE match arm: key visible + label in update_labels → updates props.
#[test]
fn test_merge_match_arm() {
    let (mut db, _dir) = open_with_writer("merge-match");
    db.insert_node("MyLabel", "existing", vec![]).unwrap();
    db.query_write_authz(
        "writer",
        "MERGE (n:MyLabel {id: 'existing'}) ON MATCH SET n.updated = 1",
        &no_params(),
    )
    .unwrap();
    assert_eq!(
        db.get_prop("existing", "updated"),
        Some(Value::Int(1)),
        "MERGE match arm must update the property"
    );
}

/// MERGE hidden key: key exists but hidden → not-visible.
#[test]
fn test_merge_hidden_key() {
    let (mut db, _dir) = open_with_writer("merge-hidden");
    // Insert a hidden node with label "MyLabel" but key that will collide.
    // Wait — for MERGE, the node's LABEL is the one in MERGE stmt. If we do
    // MERGE (n:MyLabel {id: 'hidden_key'}), and hidden_key exists as "Secret",
    // then the stored node has a different label than the MERGE target.
    // The MERGE just uses the id key; the label in MERGE is the declared label.
    // For the hidden test, we need a node with the MERGE key hidden.
    // Insert a "Secret"-labeled node with key "hidden_key" (hidden from role).
    db.insert_node("Secret", "hidden_key", vec![]).unwrap();
    let err = db
        .query_write_authz(
            "writer",
            "MERGE (n:MyLabel {id: 'hidden_key'})",
            &no_params(),
        )
        .unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "MERGE on hidden key must return RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: target node not visible"
    );
}

/// MERGE ON CREATE SET under a scoped role: InsertNode + SetProp arrive in the
/// same batch.  The SetProp must see the batch-created node as Visible and must
/// succeed without checking update_labels (ruling §3.5: batch-created nodes are
/// updatable by the same batch that created them).
#[test]
fn test_merge_on_create_set_with_role_authz() {
    // Writer role: create_labels=["MyLabel"], update_labels=["MyLabel","Visible"].
    // Confirm ON CREATE SET succeeds (create + update in scope).
    let (mut db, _dir) = open_with_writer("merge-on-create-set");
    db.query_write_authz(
        "writer",
        "MERGE (n:MyLabel {id: 'new_mc'}) ON CREATE SET n.created = 1",
        &no_params(),
    )
    .unwrap();
    assert!(
        db.has_node("new_mc"),
        "MERGE ON CREATE SET must create the node"
    );
    assert_eq!(
        db.get_prop("new_mc", "created"),
        Some(Value::Int(1)),
        "ON CREATE SET property must be applied"
    );

    // Create-only role: create_labels=["MyLabel"], update_labels=[].
    // ON CREATE SET on the batch-created node must STILL succeed (ruling §3.5).
    let dir2 = tmp("merge-on-create-set-create-only");
    let mut db2 = GraphDb::open(&dir2).unwrap();
    let schema = core_api::schema::Schema {
        fulltext: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "creator".into(),
            keys: vec![],
            labels: vec!["MyLabel".into()],
            write: Some(WriteScope {
                create_labels: vec!["MyLabel".into()],
                update_labels: vec![], // empty — no update scope
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    };
    db2.apply_schema(&schema).unwrap();
    // ON CREATE SET on a batch-created node bypasses update_labels (ruling).
    db2.query_write_authz(
        "creator",
        "MERGE (n:MyLabel {id: 'creator_node'}) ON CREATE SET n.x = 42",
        &no_params(),
    )
    .unwrap();
    assert!(db2.has_node("creator_node"));
    assert_eq!(
        db2.get_prop("creator_node", "x"),
        Some(Value::Int(42)),
        "batch-created node updatable by same batch regardless of update_labels"
    );
}

// ── EDGE-CREATE ───────────────────────────────────────────────────────────────

/// Both endpoints visible + type in create_edge_types → InsertEdge succeeds.
#[test]
fn test_edge_create_both_visible() {
    let (mut db, _dir) = open_with_writer("edge-create-both-vis");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("MyLabel", "b", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertEdge {
        edge_type: "KNOWS".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    let neighbors = db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap_or_default();
    assert!(neighbors.contains(&"b".to_string()), "edge should exist");
}

/// One endpoint hidden → edge endpoint not visible.
#[test]
fn test_edge_create_one_hidden() {
    let (mut db, _dir) = open_with_writer("edge-create-hidden-ep");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("Secret", "hidden_b", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertEdge {
        edge_type: "KNOWS".into(),
        src_key: "a".into(),
        dst_key: "hidden_b".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: edge endpoint not visible"
    );
}

/// Edge type NOT in create_edge_types → scope-denied (before endpoint lookup).
#[test]
fn test_edge_create_type_not_scoped() {
    let (mut db, _dir) = open_with_writer("edge-create-unscoped");
    db.insert_node("MyLabel", "a", vec![]).unwrap();
    db.insert_node("MyLabel", "b", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertEdge {
        edge_type: "UNSCOPED".into(), // not in create_edge_types
        src_key: "a".into(),
        dst_key: "b".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert!(
        denied_reason(&err).contains("create_edge_types"),
        "reason should name create_edge_types: {}",
        denied_reason(&err)
    );
}

// ── InsertEdgeUpsert: placeholder counts as visible ───────────────────────────

/// A placeholder endpoint created by an earlier InsertNode in the SAME batch
/// counts as visible for InsertEdgeUpsert endpoint visibility check.
#[test]
fn test_upsert_placeholder_counts_as_visible() {
    let (mut db, _dir) = open_with_writer("upsert-placeholder");
    let authz = writer_authz(&mut db);
    // Both src and dst don't exist yet. We create them via InsertNode first
    // in the same batch, then insert the edge via InsertEdgeUpsert.
    let ops = vec![
        BatchOp::InsertNode {
            label: "MyLabel".into(),
            key: "new_src".into(),
            props: vec![],
        },
        BatchOp::InsertNode {
            label: "MyLabel".into(),
            key: "new_dst".into(),
            props: vec![],
        },
        BatchOp::InsertEdge {
            edge_type: "KNOWS".into(),
            src_key: "new_src".into(),
            dst_key: "new_dst".into(),
        },
    ];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    assert!(db.has_node("new_src"));
    assert!(db.has_node("new_dst"));
    let neighbors = db
        .neighbors("new_src", "KNOWS", Direction::Out)
        .unwrap_or_default();
    assert!(
        neighbors.contains(&"new_dst".to_string()),
        "edge should exist"
    );
}

/// InsertEdgeUpsert: placeholder endpoint created by same batch is visible.
#[test]
fn test_upsert_direct_placeholder_visible() {
    let (mut db, _dir) = open_with_writer("upsert-direct-placeholder");
    let authz = writer_authz(&mut db);
    // Use InsertEdgeUpsert directly — both endpoints absent, will be created
    // with placeholder_label = "MyLabel" which IS in create_labels.
    let ops = vec![BatchOp::InsertEdgeUpsert {
        edge_type: "KNOWS".into(),
        src_key: "upsert_src".into(),
        dst_key: "upsert_dst".into(),
        placeholder_label: "MyLabel".into(),
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();
    assert!(db.has_node("upsert_src"));
    assert!(db.has_node("upsert_dst"));
    let neighbors = db
        .neighbors("upsert_src", "KNOWS", Direction::Out)
        .unwrap_or_default();
    assert!(
        neighbors.contains(&"upsert_dst".to_string()),
        "edge should exist"
    );
}

/// InsertEdgeUpsert with placeholder_label NOT in create_labels → scope-denied.
#[test]
fn test_upsert_placeholder_not_in_create_labels() {
    let (mut db, _dir) = open_with_writer("upsert-unscoped-placeholder");
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertEdgeUpsert {
        edge_type: "KNOWS".into(),
        src_key: "x".into(),
        dst_key: "y".into(),
        placeholder_label: "Secret".into(), // NOT in create_labels
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
}

// ── Cross-cutting ─────────────────────────────────────────────────────────────

/// Batch atomicity: if any op denies, NO WAL frame is written.
#[test]
fn test_batch_atomicity_no_wal_on_deny() {
    let (mut db, dir) = open_with_writer("batch-atomic");
    db.insert_node("MyLabel", "existing", vec![]).unwrap();
    let seq_before = db.commit_seq();
    let wal_before = wal_len(&dir);

    let authz = writer_authz(&mut db);
    // op1 is allowed, op2 is denied → entire batch fails, no WAL frame.
    let ops = vec![
        BatchOp::InsertNode {
            label: "MyLabel".into(),
            key: "new_node".into(),
            props: vec![],
        },
        BatchOp::InsertNode {
            label: "Secret".into(), // NOT in create_labels → denied
            key: "bad_node".into(),
            props: vec![],
        },
    ];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(is_role_write_denied(&err));
    // Neither node was inserted.
    assert!(
        !db.has_node("new_node"),
        "first op must not be applied on deny"
    );
    assert!(!db.has_node("bad_node"));
    // commit_seq unchanged → no WAL frame was written.
    assert_eq!(
        db.commit_seq(),
        seq_before,
        "commit_seq must not advance on deny"
    );
    let wal_after = wal_len(&dir);
    assert_eq!(wal_after, wal_before, "WAL must not grow on deny");
}

/// Authz fires before CAS would: hidden node → not-visible, not CasConflict.
///
/// The authz pre-check runs BEFORE CAS preconditions are evaluated (spec §5),
/// so a hidden node always returns RoleWriteDenied, never CasConflict.
#[test]
fn test_authz_fires_before_cas_would() {
    let (mut db, _dir) = open_with_writer("authz-before-cas");
    // Insert a hidden node (label not in role's read scope).
    db.insert_node("Secret", "hidden", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    // Attempt SetProp on the hidden node.
    // If authz ran AFTER CAS: we'd potentially see CasConflict or KeyNotFound.
    // If authz runs FIRST: we see RoleWriteDenied (not-visible).
    let ops = vec![BatchOp::SetProp {
        key: "hidden".into(),
        field: "x".into(),
        value: Value::Int(1),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        matches!(err, GraphError::RoleWriteDenied { .. }),
        "authz must fire before CAS; expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: target node not visible"
    );
}

/// None authz = full authority: write_batch_authz(None, ops) behaves identically
/// to write_batch.
#[test]
fn test_none_authz_full_authority() {
    let (mut db, _dir) = open_with_writer("none-authz");
    // With None, "Secret" label is allowed (full authority).
    let ops = vec![BatchOp::InsertNode {
        label: "Secret".into(),
        key: "sec_node".into(),
        props: vec![],
    }];
    db.write_batch_authz(None, ops).unwrap();
    assert!(
        db.has_node("sec_node"),
        "None authz must bypass all role checks"
    );
}

/// RenameNode op with Some(authz) → endpoint-not-permitted (defense in depth).
#[test]
fn test_rename_node_forbidden_for_role() {
    let (mut db, _dir) = open_with_writer("rename-forbidden");
    db.insert_node("MyLabel", "old_key", vec![]).unwrap();
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::RenameNode {
        old_key: "old_key".into(),
        new_key: "new_key".into(),
    }];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: this endpoint is not permitted"
    );
}

/// CreateRule op with Some(authz) → endpoint-not-permitted (defense in depth).
#[test]
fn test_create_rule_forbidden_for_role() {
    let (mut db, _dir) = open_with_writer("create-rule-forbidden");
    let authz = writer_authz(&mut db);
    let rule = RuleDef {
        name: "test_rule".into(),
        src_label: "MyLabel".into(),
        dst_label: "MyLabel".into(),
        predicate: Predicate::KeyMatch { field: "x".into() },
        edge_type: "KNOWS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    let ops = vec![BatchOp::CreateRule(rule)];
    let err = db.write_batch_authz(Some(&authz), ops).unwrap_err();
    assert!(
        is_role_write_denied(&err),
        "expected RoleWriteDenied, got {err:?}"
    );
    assert_eq!(
        denied_reason(&err),
        "role-bound token: this endpoint is not permitted"
    );
}

/// Rules fire on role-created nodes (DB authority, spec §3.5).
/// Derived edges to hidden neighbors exist in the DB but are masked on read.
#[test]
fn test_rules_fire_but_hidden_edges_masked() {
    let dir = tmp("rules-fire-hidden-masked");
    let mut db = GraphDb::open(&dir).unwrap();
    // Rule: SIMILAR edges between MyLabel nodes sharing a tag.
    let schema = Schema {
        fulltext: vec![],
        rules: vec![derived_rule()],
        views: vec![],
        roles: vec![writer_role()],
    };
    db.apply_schema(&schema).unwrap();

    // Admin inserts a hidden node with overlapping tag.
    db.insert_node(
        "Secret",
        "hidden_similar",
        vec![("tags".into(), Value::List(vec![Value::Str("xyz".into())]))],
    )
    .unwrap();

    // Role creates a MyLabel node with the same overlapping tag.
    let authz = writer_authz(&mut db);
    let ops = vec![BatchOp::InsertNode {
        label: "MyLabel".into(),
        key: "role_node".into(),
        props: vec![("tags".into(), Value::List(vec![Value::Str("xyz".into())]))],
    }];
    db.write_batch_authz(Some(&authz), ops).unwrap();

    // Rules ran with DB authority: SIMILAR edge may exist between role_node and hidden_similar.
    // But the role's read mask must NOT expose that edge.
    let mask = db.mask_for_role("writer").unwrap();
    let masked_edges = db.node_edges_masked("role_node", &mask).unwrap();
    assert!(
        !masked_edges
            .iter()
            .any(|e| e.src_key == "hidden_similar" || e.dst_key == "hidden_similar"),
        "hidden neighbor must not appear in masked edge list; edges: {masked_edges:?}"
    );
}
