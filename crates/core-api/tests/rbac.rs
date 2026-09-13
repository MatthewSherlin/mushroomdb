//! Tests for Task 1: RoleDef, Schema.roles, sidecar persistence, mask resolution.
//!
//! Test list (matches task-1-brief.md Step 1):
//! 1. apply schema with roles → diff has `role:analyst` created
//! 2. re-apply → unchanged AND roles.json byte-identical (file not touched)
//! 3. changed role → updated
//! 4. mask_for_role: keys+labels union correct
//! 5. new node of allowed label visible WITHOUT re-apply (live resolution)
//! 6. unknown role → Err
//! 7. empty role yields empty-visibility mask (query_masked returns 0 rows)
//! 8. corrupt roles.json → open succeeds but mask_for_role returns Err
//!
//! Task 1 v0.3 additions (WriteScope / sidecar v2):
//! W1. v1 file loads → all roles have write: None
//! W2. WriteScope round-trips through apply_schema + re-open
//! W3. version written is 2 only when write field present, else 1
//! W4. subset violation rejected at apply_schema with named role+label
//! W5. write-scope-only diff entry is "updated"
//! W6. unknown version (>2) still poisons
//! W7. zero-byte file is still healthy-empty

use core_api::schema::Schema;
use core_api::{GraphDb, PropPredicate, RoleDef, Value, WriteScope};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-rbac-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ))
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn analyst_role() -> RoleDef {
    RoleDef {
        name: "analyst".into(),
        keys: vec!["alice".into()],
        labels: vec!["Public".into()],
        visible_where: None,
        write: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1: apply schema with roles → diff has `role:analyst` created
// ---------------------------------------------------------------------------
#[test]
fn apply_schema_roles_creates_diff_entry() {
    let dir = tmp("roles-create");
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };

    let diff = db.apply_schema(&schema).unwrap();
    assert!(
        diff.created.contains(&"role:analyst".to_string()),
        "first apply must create role:analyst; diff: {diff:?}"
    );
    assert!(diff.updated.is_empty(), "no updates on first apply");
    assert!(diff.unchanged.is_empty(), "no unchanged on first apply");
}

// ---------------------------------------------------------------------------
// Test 2: re-apply unchanged schema → all unchanged AND roles.json byte-identical
// ---------------------------------------------------------------------------
#[test]
fn apply_schema_roles_idempotent_and_byte_identical() {
    let dir = tmp("roles-idempotent");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };

    // First apply — creates and writes roles.json.
    db.apply_schema(&schema).unwrap();
    drop(db);

    // Capture roles.json bytes after the first apply.
    let roles_path = dir.join("roles.json");
    let bytes_after_first =
        std::fs::read(&roles_path).expect("roles.json must exist after first apply");

    // Re-open and re-apply with the same schema.
    let mut db = GraphDb::open(&dir).unwrap();
    let diff = db.apply_schema(&schema).unwrap();

    assert!(
        diff.unchanged.contains(&"role:analyst".to_string()),
        "second apply must report role:analyst unchanged; diff: {diff:?}"
    );
    assert!(diff.created.is_empty(), "no creates on re-apply");
    assert!(diff.updated.is_empty(), "no updates on re-apply");
    drop(db);

    // File bytes must be identical — re-apply must not rewrite the file.
    let bytes_after_second = std::fs::read(&roles_path).expect("roles.json must still exist");
    assert_eq!(
        bytes_after_first, bytes_after_second,
        "roles.json must be byte-identical on re-apply (file was not rewritten)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: changed role → updated in diff
// ---------------------------------------------------------------------------
#[test]
fn apply_schema_role_change_triggers_update() {
    let dir = tmp("roles-update");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };
    db.apply_schema(&schema_v1).unwrap();

    let mut changed = analyst_role();
    changed.keys = vec!["bob".into()]; // different from original

    let schema_v2 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![changed],
    };
    let diff = db.apply_schema(&schema_v2).unwrap();

    assert!(
        diff.updated.contains(&"role:analyst".to_string()),
        "changed role must appear in updated; diff: {diff:?}"
    );
    assert!(diff.created.is_empty());
    assert!(diff.unchanged.is_empty());

    // In-memory roles must reflect the new key.
    let roles = db.roles();
    let live = roles
        .iter()
        .find(|r| r.name == "analyst")
        .expect("analyst must exist");
    assert_eq!(
        live.keys,
        vec!["bob"],
        "role keys must be updated in memory"
    );
}

// ---------------------------------------------------------------------------
// Test 4: mask_for_role keys+labels union
// ---------------------------------------------------------------------------
#[test]
fn mask_for_role_keys_and_labels_union() {
    let dir = tmp("mask-union");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Public", "alice", vec![]).unwrap(); // label-visible
    db.insert_node("Public", "bob", vec![]).unwrap(); // label-visible
    db.insert_node("Private", "secret", vec![]).unwrap(); // neither key nor label

    // Role: explicit key "alice" (key leg) + label "Public" (label leg)
    // Union: alice (key) + alice,bob (label) = alice + bob
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "viewer".into(),
            keys: vec!["alice".into()],
            labels: vec!["Public".into()],
            visible_where: None,
            write: None,
        }],
    };
    db.apply_schema(&schema).unwrap();

    let mask = db.mask_for_role("viewer").unwrap();
    assert_eq!(
        mask.len(),
        2,
        "union of key+label should give alice+bob (2 nodes)"
    );

    // Query with mask: should see alice and bob, not secret.
    let rs = db
        .query_masked("MATCH (n) RETURN n.id", &no_params(), &mask)
        .unwrap();
    assert_eq!(rs.len(), 2, "masked query should return 2 rows");

    // secret must not appear.
    let mut found_secret = false;
    for i in 0..rs.len() {
        let row = rs.row(i);
        if let Some(Some(v)) = row.first() {
            if format!("{v:?}").contains("secret") {
                found_secret = true;
            }
        }
    }
    assert!(!found_secret, "secret must not be visible");
}

// ---------------------------------------------------------------------------
// Test 5: new node of allowed label visible WITHOUT re-apply (live resolution)
// ---------------------------------------------------------------------------
#[test]
fn mask_for_role_label_resolves_live() {
    let dir = tmp("mask-live");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Public", "alice", vec![]).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "viewer".into(),
            keys: vec![],
            labels: vec!["Public".into()],
            visible_where: None,
            write: None,
        }],
    };
    db.apply_schema(&schema).unwrap();

    // Verify alice is visible.
    let mask = db.mask_for_role("viewer").unwrap();
    assert_eq!(mask.len(), 1, "only alice initially");

    // Insert a new Public node WITHOUT re-applying the schema.
    db.insert_node("Public", "bob", vec![]).unwrap();

    // Re-resolve the mask — bob must be visible immediately.
    let mask2 = db.mask_for_role("viewer").unwrap();
    assert_eq!(mask2.len(), 2, "bob must be visible without re-apply");

    let rs = db
        .query_masked("MATCH (n:Public) RETURN n.id", &no_params(), &mask2)
        .unwrap();
    assert_eq!(
        rs.len(),
        2,
        "both alice and bob must appear in masked query"
    );
}

// ---------------------------------------------------------------------------
// Test 6: unknown role → Err
// ---------------------------------------------------------------------------
#[test]
fn mask_for_role_unknown_role_returns_err() {
    let dir = tmp("mask-unknown");
    let _ = std::fs::remove_dir_all(&dir);
    let db = GraphDb::open(&dir).unwrap();

    let result = db.mask_for_role("nonexistent");
    assert!(result.is_err(), "unknown role must return Err");
}

// ---------------------------------------------------------------------------
// Test 7: empty role → empty mask → query_masked returns 0 rows
// ---------------------------------------------------------------------------
#[test]
fn empty_role_yields_empty_visibility() {
    let dir = tmp("mask-empty-role");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "nothing".into(),
            keys: vec![],
            labels: vec![],
            visible_where: None,
            write: None,
        }],
    };
    db.apply_schema(&schema).unwrap();

    let mask = db.mask_for_role("nothing").unwrap();
    assert!(mask.is_empty(), "empty role must produce empty mask");

    let rs = db
        .query_masked("MATCH (n:P) RETURN n.id", &no_params(), &mask)
        .unwrap();
    assert_eq!(rs.len(), 0, "empty mask must hide all nodes");
}

// ---------------------------------------------------------------------------
// Test 8: corrupt roles.json → open succeeds but mask_for_role returns Err
// ---------------------------------------------------------------------------
#[test]
fn corrupt_roles_json_open_succeeds_mask_for_role_errs() {
    let dir = tmp("mask-corrupt");
    let _ = std::fs::remove_dir_all(&dir);

    // Open once to create the directory.
    let mut db = GraphDb::open(&dir).unwrap();
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };
    db.apply_schema(&schema).unwrap();
    drop(db);

    // Overwrite roles.json with invalid JSON.
    std::fs::write(dir.join("roles.json"), b"this is not valid json").unwrap();

    // Re-open: must succeed despite corrupt file.
    let db = GraphDb::open(&dir).unwrap();

    // mask_for_role must return Err (fail-loud: never silently grant empty mask).
    let result = db.mask_for_role("analyst");
    assert!(
        result.is_err(),
        "corrupt roles.json must cause mask_for_role to return Err"
    );

    // Unknown role also returns Err (same poisoned state).
    let result2 = db.mask_for_role("nonexistent");
    assert!(
        result2.is_err(),
        "all role requests must fail when roles.json is corrupt"
    );
}

// ---------------------------------------------------------------------------
// Bonus: validation — empty role name and duplicate role name are rejected
// ---------------------------------------------------------------------------
#[test]
fn apply_schema_rejects_empty_role_name() {
    let dir = tmp("roles-validate-empty");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "".into(),
            keys: vec![],
            labels: vec![],
            visible_where: None,
            write: None,
        }],
    };
    assert!(
        db.apply_schema(&schema).is_err(),
        "empty role name must be rejected"
    );
}

#[test]
fn apply_schema_rejects_duplicate_role_names() {
    let dir = tmp("roles-validate-dup");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "viewer".into(),
                keys: vec![],
                labels: vec![],
                visible_where: None,
                write: None,
            },
            RoleDef {
                name: "viewer".into(),
                keys: vec!["alice".into()],
                labels: vec![],
                visible_where: None,
                write: None,
            },
        ],
    };
    assert!(
        db.apply_schema(&schema).is_err(),
        "duplicate role names must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Bonus: roles() returns current list; empty on no roles
// ---------------------------------------------------------------------------
#[test]
fn roles_accessor_returns_defined_roles() {
    let dir = tmp("roles-accessor");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    assert!(db.roles().is_empty(), "no roles initially");

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };
    db.apply_schema(&schema).unwrap();

    let roles = db.roles();
    assert_eq!(roles.len(), 1);
    assert_eq!(roles[0].name, "analyst");
}

// ---------------------------------------------------------------------------
// Bonus: roles persist across re-open
// ---------------------------------------------------------------------------
#[test]
fn roles_survive_reopen() {
    let dir = tmp("roles-persist");
    let _ = std::fs::remove_dir_all(&dir);

    {
        let mut db = GraphDb::open(&dir).unwrap();
        let schema = Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![analyst_role()],
        };
        db.apply_schema(&schema).unwrap();
    }

    // Re-open: roles must be loaded from roles.json.
    let db = GraphDb::open(&dir).unwrap();
    let roles = db.roles();
    assert_eq!(roles.len(), 1, "roles must survive re-open");
    assert_eq!(roles[0].name, "analyst");
    assert_eq!(roles[0].keys, vec!["alice"]);
    assert_eq!(roles[0].labels, vec!["Public"]);

    // mask_for_role must also work after re-open.
    let mask = db.mask_for_role("analyst");
    // (No nodes in this db, so mask resolves to empty — that's correct)
    assert!(mask.is_ok(), "mask_for_role must work after re-open");
}

// ---------------------------------------------------------------------------
// Item 21: Repair path — apply_schema over a corrupt sidecar heals the state
// ---------------------------------------------------------------------------

/// When roles.json is corrupt at open (poisoning the state so mask_for_role
/// returns Err), calling apply_schema with a valid schema must repair the
/// sidecar and restore mask_for_role to Ok.
#[test]
fn apply_schema_over_corrupt_sidecar_repairs_roles() {
    let dir = tmp("roles-repair");
    let _ = std::fs::remove_dir_all(&dir);

    // Initial good state.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        let schema = Schema {
            fulltext: vec![],
            indexes: vec![],
            rules: vec![],
            views: vec![],
            roles: vec![analyst_role()],
        };
        db.apply_schema(&schema).unwrap();
    }

    // Corrupt roles.json.
    std::fs::write(dir.join("roles.json"), b"not valid json").unwrap();

    // Re-open: succeeds but mask_for_role is poisoned.
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(
        db.mask_for_role("analyst").is_err(),
        "mask_for_role must fail with corrupt roles.json"
    );

    // Repair: apply schema with valid roles — writes a fresh roles.json.
    let repair_schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };
    db.apply_schema(&repair_schema)
        .expect("apply_schema over corrupt sidecar must succeed");

    // State is now repaired — mask_for_role must return Ok.
    assert!(
        db.mask_for_role("analyst").is_ok(),
        "mask_for_role must succeed after repair via apply_schema"
    );
}

// ---------------------------------------------------------------------------
// W1: v1 file loads → all roles have write: None
// ---------------------------------------------------------------------------
#[test]
fn v1_file_loads_all_roles_write_none() {
    let dir = tmp("v1-write-none");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Write a valid v1 roles.json manually (no write field).
    let v1_json =
        r#"{"version":1,"roles":[{"name":"analyst","keys":["alice"],"labels":["Public"]}]}"#;
    std::fs::write(dir.join("roles.json"), v1_json).unwrap();

    let db = GraphDb::open(&dir).unwrap();
    let roles = db.roles();
    assert_eq!(roles.len(), 1, "v1 file must load one role");
    assert!(
        roles[0].write.is_none(),
        "role loaded from v1 sidecar must have write: None"
    );
}

// ---------------------------------------------------------------------------
// W2: WriteScope round-trips through apply_schema + re-open
// ---------------------------------------------------------------------------
#[test]
fn v2_write_scope_round_trips() {
    let dir = tmp("v2-roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "agent-memory".into(),
        keys: vec![],
        labels: vec!["AgentNote".into(), "AgentContext".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec!["AgentNote".into(), "AgentContext".into()],
            update_labels: vec!["AgentNote".into()],
            delete_labels: vec!["AgentNote".into()],
            create_edge_types: vec!["RECALLS".into()],
            delete_edge_types: vec!["RECALLS".into()],
        }),
    };

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role.clone()],
    };
    db.apply_schema(&schema).unwrap();
    drop(db);

    // Re-open and verify fields are preserved.
    let db = GraphDb::open(&dir).unwrap();
    let roles = db.roles();
    assert_eq!(roles.len(), 1);
    let loaded = &roles[0];
    assert_eq!(loaded.name, "agent-memory");
    let ws = loaded
        .write
        .as_ref()
        .expect("write scope must survive re-open");
    assert_eq!(ws.create_labels, vec!["AgentNote", "AgentContext"]);
    assert_eq!(ws.update_labels, vec!["AgentNote"]);
    assert_eq!(ws.delete_labels, vec!["AgentNote"]);
    assert_eq!(ws.create_edge_types, vec!["RECALLS"]);
    assert_eq!(ws.delete_edge_types, vec!["RECALLS"]);
}

// ---------------------------------------------------------------------------
// W3a: version written is 2 when any role has a write field
// ---------------------------------------------------------------------------
#[test]
fn version_written_is_v2_when_write_present() {
    let dir = tmp("v2-version-pin-write");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "writer".into(),
        keys: vec![],
        labels: vec!["AgentNote".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec!["AgentNote".into()],
            update_labels: vec![],
            delete_labels: vec![],
            create_edge_types: vec![],
            delete_edge_types: vec![],
        }),
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };
    db.apply_schema(&schema).unwrap();
    drop(db);

    let bytes = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["version"].as_u64().unwrap(),
        2,
        "roles.json version must be 2 when any role has a write field"
    );
}

// ---------------------------------------------------------------------------
// W3b: version written is 1 when no role has a write field
// ---------------------------------------------------------------------------
#[test]
fn version_written_is_v1_when_no_write() {
    let dir = tmp("v1-version-pin-nowrite");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()], // write: None
    };
    db.apply_schema(&schema).unwrap();
    drop(db);

    let bytes = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["version"].as_u64().unwrap(),
        1,
        "roles.json version must be 1 when no role has a write field"
    );
}

// ---------------------------------------------------------------------------
// W4: subset violation rejected at apply_schema with named role + label
// ---------------------------------------------------------------------------
#[test]
fn subset_violation_create_labels_not_in_read_labels_rejected() {
    let dir = tmp("subset-create");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    // "Secret" is not in labels, but is in create_labels — should be rejected.
    let role = RoleDef {
        name: "agent".into(),
        keys: vec![],
        labels: vec!["AgentNote".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec!["AgentNote".into(), "Secret".into()],
            update_labels: vec![],
            delete_labels: vec![],
            create_edge_types: vec![],
            delete_edge_types: vec![],
        }),
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };

    let err = db.apply_schema(&schema).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("agent"),
        "error must name the role; got: {msg}"
    );
    assert!(
        msg.contains("Secret"),
        "error must name the offending label; got: {msg}"
    );
}

#[test]
fn subset_violation_update_labels_not_in_read_labels_rejected() {
    let dir = tmp("subset-update");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "editor".into(),
        keys: vec![],
        labels: vec!["Doc".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec!["Doc".into()],
            update_labels: vec!["Hidden".into()], // not in labels
            delete_labels: vec![],
            create_edge_types: vec![],
            delete_edge_types: vec![],
        }),
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };

    let err = db.apply_schema(&schema).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("editor"),
        "error must name the role; got: {msg}"
    );
    assert!(
        msg.contains("Hidden"),
        "error must name the offending label; got: {msg}"
    );
}

#[test]
fn subset_violation_delete_labels_not_in_read_labels_rejected() {
    let dir = tmp("subset-delete");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "deleter".into(),
        keys: vec![],
        labels: vec!["Doc".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec![],
            update_labels: vec![],
            delete_labels: vec!["AdminDoc".into()], // not in labels
            create_edge_types: vec![],
            delete_edge_types: vec![],
        }),
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };

    let err = db.apply_schema(&schema).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("deleter"),
        "error must name the role; got: {msg}"
    );
    assert!(
        msg.contains("AdminDoc"),
        "error must name the offending label; got: {msg}"
    );
}

#[test]
fn edge_types_not_subset_validated() {
    // create_edge_types / delete_edge_types have no subset requirement — must succeed.
    let dir = tmp("no-subset-edge-types");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "linker".into(),
        keys: vec![],
        labels: vec!["Doc".into()],
        visible_where: None,
        write: Some(WriteScope {
            create_labels: vec![],
            update_labels: vec![],
            delete_labels: vec![],
            create_edge_types: vec!["LINKS_TO".into(), "ANYTHING".into()], // arbitrary
            delete_edge_types: vec!["WHATEVER".into()],                    // arbitrary
        }),
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };

    db.apply_schema(&schema)
        .expect("edge types do not require subset validation — must succeed");
}

// ---------------------------------------------------------------------------
// W5: write-scope-only change produces "updated" diff entry
// ---------------------------------------------------------------------------
#[test]
fn write_scope_only_change_produces_updated_diff() {
    let dir = tmp("ws-only-updated");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    // First apply: read-only role.
    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "scoped".into(),
            keys: vec![],
            labels: vec!["Doc".into()],
            visible_where: None,
            write: None,
        }],
    };
    let diff1 = db.apply_schema(&schema_v1).unwrap();
    assert!(diff1.created.contains(&"role:scoped".to_string()));

    // Second apply: add write scope (same read scope).
    let schema_v2 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "scoped".into(),
            keys: vec![],
            labels: vec!["Doc".into()],
            visible_where: None,
            write: Some(WriteScope {
                create_labels: vec!["Doc".into()],
                update_labels: vec![],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
    };
    let diff2 = db.apply_schema(&schema_v2).unwrap();
    assert!(
        diff2.updated.contains(&"role:scoped".to_string()),
        "write-scope-only addition must appear in updated; diff: {diff2:?}"
    );
    assert!(diff2.created.is_empty());
    assert!(diff2.unchanged.is_empty());
}

// ---------------------------------------------------------------------------
// W6: unknown version (>2) still poisons
// ---------------------------------------------------------------------------
#[test]
fn unknown_version_greater_than_two_poisons() {
    let dir = tmp("v99-poison");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let v99_json = r#"{"version":99,"roles":[{"name":"analyst","labels":["Public"]}]}"#;
    std::fs::write(dir.join("roles.json"), v99_json).unwrap();

    let db = GraphDb::open(&dir).unwrap();
    let result = db.mask_for_role("analyst");
    assert!(
        result.is_err(),
        "version 99 roles.json must poison the state — mask_for_role must return Err"
    );
}

// ---------------------------------------------------------------------------
// W7: zero-byte file is still healthy-empty (no poison)
// ---------------------------------------------------------------------------
#[test]
fn zero_byte_roles_json_is_healthy_empty() {
    let dir = tmp("zero-byte-healthy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Write a zero-byte roles.json.
    std::fs::write(dir.join("roles.json"), b"").unwrap();

    let db = GraphDb::open(&dir).unwrap();
    // roles() must return empty list (not poisoned).
    assert!(
        db.roles().is_empty(),
        "zero-byte roles.json must give empty roles list"
    );
    // mask_for_role for an unknown role returns KeyNotFound, not a corruption error.
    let result = db.mask_for_role("nobody");
    let err = match result {
        Ok(_) => panic!("expected Err for unknown role, got Ok"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        !msg.contains("corrupt"),
        "zero-byte file must not produce a corruption error; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// W3c: write: Some(WriteScope::default()) — all vecs empty — still lifts to v2
// and round-trips back as Some(empty), not None.
// ---------------------------------------------------------------------------
#[test]
fn empty_write_scope_still_writes_v2_and_round_trips_as_some() {
    let dir = tmp("v2-empty-write-scope");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let role = RoleDef {
        name: "noop-writer".into(),
        keys: vec![],
        labels: vec!["Doc".into()],
        visible_where: None,
        write: Some(WriteScope::default()), // all vecs empty, but Some(...)
    };
    let schema = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![role],
    };
    db.apply_schema(&schema).unwrap();
    drop(db);

    // Version in file must be 2 — write is Some, even if all fields are empty.
    let bytes = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["version"].as_u64().unwrap(),
        2,
        "Some(WriteScope::default()) must produce version 2, not 1"
    );

    // On reload, write must come back as Some with all-empty vecs, not None.
    let db = GraphDb::open(&dir).unwrap();
    let roles = db.roles();
    assert_eq!(roles.len(), 1);
    let ws = roles[0]
        .write
        .as_ref()
        .expect("write must round-trip as Some, not coerce to None");
    assert!(ws.create_labels.is_empty());
    assert!(ws.update_labels.is_empty());
    assert!(ws.delete_labels.is_empty());
    assert!(ws.create_edge_types.is_empty());
    assert!(ws.delete_edge_types.is_empty());
}

// ---------------------------------------------------------------------------
// W2b: multi-role v1→v2 transition — two roles, apply_schema adds write to
// only one; file lifts from v1 to v2; writeless role round-trips unchanged.
// ---------------------------------------------------------------------------
#[test]
fn multi_role_v1_to_v2_transition_writeless_role_unchanged() {
    let dir = tmp("multi-role-v1-to-v2");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    // First apply: two read-only roles (v1).
    let schema_v1 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "reader".into(),
                keys: vec!["key1".into()],
                labels: vec!["Public".into()],
                visible_where: None,
                write: None,
            },
            RoleDef {
                name: "admin".into(),
                keys: vec!["key2".into()],
                labels: vec!["Admin".into()],
                visible_where: None,
                write: None,
            },
        ],
    };
    db.apply_schema(&schema_v1).unwrap();
    drop(db);

    let bytes_v1 = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed_v1: serde_json::Value = serde_json::from_slice(&bytes_v1).unwrap();
    assert_eq!(
        parsed_v1["version"].as_u64().unwrap(),
        1,
        "initial apply with no write scopes must write version 1"
    );

    // Second apply: give admin a write scope; reader stays read-only.
    let mut db = GraphDb::open(&dir).unwrap();
    let schema_v2 = Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "reader".into(),
                keys: vec!["key1".into()],
                labels: vec!["Public".into()],
                visible_where: None,
                write: None, // unchanged
            },
            RoleDef {
                name: "admin".into(),
                keys: vec!["key2".into()],
                labels: vec!["Admin".into()],
                visible_where: None,
                write: Some(WriteScope {
                    create_labels: vec!["Admin".into()],
                    update_labels: vec![],
                    delete_labels: vec![],
                    create_edge_types: vec![],
                    delete_edge_types: vec![],
                }),
            },
        ],
    };
    let diff = db.apply_schema(&schema_v2).unwrap();
    assert!(
        diff.updated.contains(&"role:admin".to_string()),
        "admin must be updated; diff: {diff:?}"
    );
    assert!(
        diff.unchanged.contains(&"role:reader".to_string()),
        "reader must be unchanged; diff: {diff:?}"
    );
    drop(db);

    // File must now be version 2.
    let bytes_v2 = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed_v2: serde_json::Value = serde_json::from_slice(&bytes_v2).unwrap();
    assert_eq!(
        parsed_v2["version"].as_u64().unwrap(),
        2,
        "file must lift to version 2 after adding write scope to one role"
    );

    // Re-open: reader must still have write: None, keys and labels intact.
    let db = GraphDb::open(&dir).unwrap();
    let roles = db.roles();
    let reader = roles
        .iter()
        .find(|r| r.name == "reader")
        .expect("reader must survive");
    assert!(reader.write.is_none(), "reader.write must remain None");
    assert_eq!(reader.keys, vec!["key1"], "reader.keys must be intact");
    assert_eq!(
        reader.labels,
        vec!["Public"],
        "reader.labels must be intact"
    );

    // admin must have the write scope preserved.
    let admin = roles
        .iter()
        .find(|r| r.name == "admin")
        .expect("admin must survive");
    let ws = admin
        .write
        .as_ref()
        .expect("admin.write must be Some after v2 reload");
    assert_eq!(ws.create_labels, vec!["Admin"]);
}

// ---------------------------------------------------------------------------
// Task 6 (v0.6.5): predicate masks (`visible_where`) and the per-commit memo.
// ---------------------------------------------------------------------------

/// Resolve `role` and return the keys it can actually read, sorted.
fn mask_keys(db: &GraphDb<core_storage::fs::RealFs>, role: &str) -> Vec<String> {
    let mask = db.mask_for_role(role).expect("mask_for_role");
    let rs = db
        .query_masked("MATCH (n) RETURN n", &no_params(), &mask)
        .expect("masked query");
    let mut keys: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.row(i)[0].as_ref() {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    keys.sort();
    keys
}

fn published() -> PropPredicate {
    PropPredicate {
        field: "status".into(),
        eq: None,
        in_: Some(vec![Value::Str("published".into())]),
    }
}

fn reader_role(vw: Option<PropPredicate>, keys: Vec<String>) -> RoleDef {
    RoleDef {
        name: "reader".into(),
        keys,
        labels: vec!["Document".into()],
        visible_where: vw,
        write: None,
    }
}

fn roles_schema(roles: Vec<RoleDef>) -> Schema {
    Schema {
        roles,
        ..Default::default()
    }
}

/// `visible_where` narrows the labels leg and never the keys leg.
#[test]
fn visible_where_narrows_the_label_leg() {
    let dir = tmp("predicate-mask");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "draft",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap();
    db.insert_node("Document", "bare", vec![]).unwrap(); // no status at all
    db.insert_node(
        "Secret",
        "s",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();

    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();
    assert_eq!(
        mask_keys(&db, "reader"),
        vec!["pub"],
        "draft fails the predicate; bare has no status and absent is not a match; \
         Secret is not in labels"
    );

    // keys is an administrative grant and is never narrowed.
    db.apply_schema(&roles_schema(vec![reader_role(
        Some(published()),
        vec!["draft".into()],
    )]))
    .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["draft", "pub"]);

    // `eq` is the single-value form.
    db.apply_schema(&roles_schema(vec![reader_role(
        Some(PropPredicate {
            field: "status".into(),
            eq: Some(Value::Str("draft".into())),
            in_: None,
        }),
        vec![],
    )]))
    .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["draft"]);

    // An empty `in` matches nothing.
    db.apply_schema(&roles_schema(vec![reader_role(
        Some(PropPredicate {
            field: "status".into(),
            eq: None,
            in_: Some(vec![]),
        }),
        vec![],
    )]))
    .unwrap();
    assert!(
        mask_keys(&db, "reader").is_empty(),
        "empty `in` matches nothing"
    );

    // No `visible_where` at all is exactly the pre-v3 behaviour.
    db.apply_schema(&roles_schema(vec![reader_role(None, vec![])]))
        .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["bare", "draft", "pub"]);

    // Neither `eq` nor `in` is not a predicate.
    assert!(
        db.apply_schema(&roles_schema(vec![reader_role(
            Some(PropPredicate {
                field: "status".into(),
                eq: None,
                in_: None,
            }),
            vec![],
        )]))
        .is_err(),
        "a predicate with neither eq nor in must be refused"
    );

    // Both at once is ambiguous.
    assert!(
        db.apply_schema(&roles_schema(vec![reader_role(
            Some(PropPredicate {
                field: "status".into(),
                eq: Some(Value::Str("x".into())),
                in_: Some(vec![Value::Str("y".into())]),
            }),
            vec![],
        )]))
        .is_err(),
        "a predicate with both eq and in must be refused"
    );

    // An empty field name names nothing.
    assert!(
        db.apply_schema(&roles_schema(vec![reader_role(
            Some(PropPredicate {
                field: String::new(),
                eq: Some(Value::Str("x".into())),
                in_: None,
            }),
            vec![],
        )]))
        .is_err(),
        "an empty field must be refused"
    );

    // A visible_where with no labels is a mistake, not a no-op.
    assert!(
        db.apply_schema(&roles_schema(vec![RoleDef {
            name: "r2".into(),
            keys: vec![],
            labels: vec![],
            visible_where: Some(PropPredicate {
                field: "status".into(),
                eq: Some(Value::Str("x".into())),
                in_: None,
            }),
            write: None,
        }]))
        .is_err(),
        "visible_where with no labels must be refused"
    );
}

/// A non-string predicate value compares by value, not by rendering.
#[test]
fn visible_where_matches_non_string_values() {
    let dir = tmp("predicate-nonstring");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Document", "n1", vec![("tier".into(), Value::Int(1))])
        .unwrap();
    db.insert_node("Document", "n2", vec![("tier".into(), Value::Int(2))])
        .unwrap();
    db.insert_node("Document", "n3", vec![("tier".into(), Value::Bool(true))])
        .unwrap();

    db.apply_schema(&roles_schema(vec![reader_role(
        Some(PropPredicate {
            field: "tier".into(),
            eq: None,
            in_: Some(vec![Value::Int(2), Value::Bool(true)]),
        }),
        vec![],
    )]))
    .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["n2", "n3"]);
}

/// `roles.json` version 3 round-trips; an unchanged v2 file still loads.
#[test]
fn roles_json_version_3_round_trips() {
    let dir = tmp("roles-v3");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "draft",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap();

    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["pub"]);
    drop(db);

    let bytes = std::fs::read(dir.join("roles.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        parsed["version"].as_u64().unwrap(),
        3,
        "a role carrying visible_where lifts the sidecar to version 3"
    );
    // Serialization is always the tagged form, whichever spelling was typed —
    // the shape docs/site/masks.md calls "what the server writes back".
    assert_eq!(
        parsed["roles"][0]["visible_where"],
        serde_json::json!({"field": "status", "in": [{"Str": "published"}]}),
        "the predicate is written back in the graph's tagged value encoding"
    );

    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.roles()[0].visible_where.as_ref().unwrap(),
        &published(),
        "the predicate must survive the round trip"
    );
    assert_eq!(
        mask_keys(&db, "reader"),
        vec!["pub"],
        "and resolve the same"
    );
}

/// A version 2 sidecar written before predicates existed still loads, and a
/// role without `visible_where` behaves exactly as it did.
#[test]
fn roles_json_version_2_still_loads() {
    let dir = tmp("roles-v2-compat");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("roles.json"),
        br#"{"version":2,"roles":[{"name":"reader","keys":["k"],"labels":["Document"],
             "write":{"create_labels":["Document"],"update_labels":[],"delete_labels":[],
                      "create_edge_types":[],"delete_edge_types":[]}}]}"#,
    )
    .unwrap();

    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.insert_node("Document", "bare", vec![]).unwrap();

    let roles = db.roles();
    assert!(
        roles[0].visible_where.is_none(),
        "a v2 role has no predicate"
    );
    assert!(roles[0].write.is_some(), "and keeps its write scope");
    assert_eq!(
        mask_keys(&db, "reader"),
        vec!["bare", "pub"],
        "no predicate = every node of the label, exactly as before"
    );
}

/// Version 4 and up still poisons — never-widen holds in the new direction too.
#[test]
fn roles_json_version_4_poisons() {
    let dir = tmp("roles-v4-poison");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("roles.json"),
        br#"{"version":4,"roles":[{"name":"reader","labels":["Document"]}]}"#,
    )
    .unwrap();

    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.mask_for_role("reader").is_err(),
        "version 4 must poison the roles state"
    );
}

/// The memo is keyed by commit sequence, so a write invalidates it.
#[test]
fn mask_memo_is_invalidated_by_a_write() {
    let dir = tmp("mask-memo");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();

    assert_eq!(mask_keys(&db, "reader"), vec!["pub"]);

    // A write the predicate admits. A stale memo would still answer "pub".
    db.insert_node(
        "Document",
        "pub2",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["pub", "pub2"]);

    // A property change that takes a node out of the predicate is seen too.
    db.set_prop("pub", "status", Value::Str("draft".into()))
        .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["pub2"]);

    // Narrowing the role itself is not a commit, and must still be seen.
    db.apply_schema(&roles_schema(vec![reader_role(
        Some(PropPredicate {
            field: "status".into(),
            eq: Some(Value::Str("draft".into())),
            in_: None,
        }),
        vec![],
    )]))
    .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["pub"]);
}

/// The reader snapshot resolves the same predicate mask as the live handle.
#[test]
fn reader_snapshot_honours_the_predicate() {
    let dir = tmp("mask-memo-reader");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "draft",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap();
    db.insert_node("Document", "bare", vec![]).unwrap(); // no status at all
    db.apply_schema(&roles_schema(vec![reader_role(
        Some(published()),
        vec!["draft".into()],
    )]))
    .unwrap();

    // The reader-side resolver is the live one's twin: `pub` passes, `bare` has
    // no status and absent is not a match, and the `draft` key is granted
    // administratively and never narrowed.
    let snap = db.reader();
    let mask = snap.mask_for_role("reader").unwrap();
    assert_eq!(mask.len(), 2, "reader snapshot applies the predicate too");
    let rs = snap
        .query_masked("MATCH (n) RETURN n", &no_params(), &mask)
        .unwrap();
    let mut seen: Vec<String> = (0..rs.len())
        .filter_map(|i| match rs.row(i)[0].as_ref() {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    seen.sort();
    assert_eq!(seen, vec!["draft", "pub"]);
    assert_eq!(
        seen,
        mask_keys(&db, "reader"),
        "the snapshot and the live handle resolve the same role identically"
    );

    // A snapshot taken after a write sees the new node; the memo is per-commit.
    db.insert_node(
        "Document",
        "pub2",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    let snap2 = db.reader();
    assert_eq!(snap2.mask_for_role("reader").unwrap().len(), 3);
    // The old snapshot still answers for the state it froze.
    assert_eq!(snap.mask_for_role("reader").unwrap().len(), 2);
}

/// A role edit is not a commit, so the memo's version key cannot see it. The
/// live handle takes a fresh cache, which leaves a snapshot frozen against the
/// old definitions unable to publish its now-wrong mask into it.
#[test]
fn a_stale_snapshot_cannot_publish_its_mask_after_a_role_edit() {
    let dir = tmp("mask-memo-role-edit");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "draft",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap();
    db.apply_schema(&roles_schema(vec![reader_role(None, vec![])]))
        .unwrap();

    // Frozen while the role is still the wide one.
    let stale = db.reader();
    let before = db.commit_seq();

    // Narrow the role. Rewriting the sidecar is not a commit.
    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();
    assert_eq!(
        db.commit_seq(),
        before,
        "a role edit must not move commit_seq — that is exactly why the memo \
         cannot rely on it here"
    );

    // The stale snapshot resolves FIRST, against the definitions it froze. If it
    // shared the live memo it would seed the wide mask under the current version.
    assert_eq!(
        stale.mask_for_role("reader").unwrap().len(),
        2,
        "the snapshot answers for the role definition it froze"
    );

    // The live handle must still be narrowed.
    assert_eq!(
        mask_keys(&db, "reader"),
        vec!["pub"],
        "a stale snapshot must not be able to widen the live handle's answer"
    );
}

/// An as-of read applies the current predicate to the historical graph.
#[test]
fn visible_where_applies_to_an_as_of_read() {
    use core_api::AsOfScope;

    let dir = tmp("predicate-asof");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node(
        "Document",
        "pub",
        vec![("status".into(), Value::Str("published".into()))],
    )
    .unwrap(); // commit 0
    db.insert_node(
        "Document",
        "draft",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap(); // commit 1
    db.set_prop("draft", "status", Value::Str("published".into()))
        .unwrap(); // commit 2
    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();

    // At the latest commit both are published.
    let rs = db
        .query_at_scoped(
            2,
            "MATCH (n) RETURN n",
            &no_params(),
            AsOfScope::Role("reader"),
        )
        .unwrap();
    assert_eq!(rs.len(), 2, "both documents are published at commit 2");

    // At commit 1 `draft` was still a draft — the predicate evaluates against
    // the property values AT the commit being read.
    let rs = db
        .query_at_scoped(
            1,
            "MATCH (n) RETURN n",
            &no_params(),
            AsOfScope::Role("reader"),
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "draft fails the predicate at commit 1");
}

/// Measurement, not an assertion: the cost of a scoped read's mask resolution
/// with the memo cold versus warm, on a store with ~2k readable nodes.
///
/// Run with `cargo test -p mushroomdb --test rbac -- --ignored --nocapture`.
/// Debug build numbers; they are a ratio, not a throughput claim.
#[test]
#[ignore = "measurement, printed not asserted"]
fn mask_memo_cost_before_and_after() {
    let dir = tmp("mask-memo-cost");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    db.write_batch(|b| {
        for i in 0..2000 {
            b.insert_node(
                "Document",
                &format!("d{i}"),
                vec![("status".into(), Value::Str("published".into()))],
            );
        }
        for i in 0..500 {
            b.insert_node(
                "Document",
                &format!("x{i}"),
                vec![("status".into(), Value::Str("draft".into()))],
            );
        }
    })
    .unwrap();
    db.apply_schema(&roles_schema(vec![reader_role(Some(published()), vec![])]))
        .unwrap();

    // Cold: every resolve follows a write, so the memo never matches.
    let reps = 200;
    let cold = {
        let mut total = std::time::Duration::ZERO;
        for i in 0..reps {
            // The write is what invalidates the memo; it is not what is timed.
            db.set_prop(&format!("d{i}"), "touched", Value::Int(i as i64))
                .unwrap();
            let start = std::time::Instant::now();
            let m = db.mask_for_role("reader").unwrap();
            total += start.elapsed();
            assert_eq!(m.len(), 2000);
        }
        total
    };

    // Warm: no write between resolves, so the memo answers every time.
    let warm = {
        let start = std::time::Instant::now();
        for _ in 0..reps {
            let m = db.mask_for_role("reader").unwrap();
            assert_eq!(m.len(), 2000);
        }
        start.elapsed()
    };

    eprintln!(
        "mask_for_role over 2000 readable / 2500 total nodes, {reps} resolves (debug build):\n  \
         cold (a write before each): {:?} total, {:?} each\n  \
         warm (memo hit):            {:?} total, {:?} each",
        cold,
        cold / reps,
        warm,
        warm / reps,
    );
}

/// A predicate value may be written as a plain JSON scalar, not only in the
/// graph's tagged `Value` form. A hand-written `roles.json` is the normal case,
/// and getting it wrong poisons every role in the store.
#[test]
fn visible_where_accepts_plain_json_scalars() {
    let dir = tmp("predicate-untagged");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Written by hand, in the shape the spec and the docs show.
    std::fs::write(
        dir.join("roles.json"),
        br#"{"version":3,"roles":[
             {"name":"reader","keys":[],"labels":["Document"],
              "visible_where":{"field":"status","in":["published","archived"]}},
             {"name":"core","keys":[],"labels":["Document"],
              "visible_where":{"field":"kind","eq":"core"}},
             {"name":"tier","keys":[],"labels":["Document"],
              "visible_where":{"field":"tier","in":[2,{"Int":3}]}},
             {"name":"flagged","keys":[],"labels":["Document"],
              "visible_where":{"field":"flag","eq":true}}
           ]}"#,
    )
    .unwrap();

    let mut db = GraphDb::open(&dir).unwrap();
    assert!(
        !db.roles().is_empty(),
        "an untagged predicate must parse, not poison the sidecar"
    );

    db.insert_node(
        "Document",
        "a",
        vec![
            ("status".into(), Value::Str("published".into())),
            ("kind".into(), Value::Str("core".into())),
            ("tier".into(), Value::Int(2)),
            ("flag".into(), Value::Bool(true)),
        ],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "b",
        vec![
            ("status".into(), Value::Str("archived".into())),
            ("kind".into(), Value::Str("extra".into())),
            ("tier".into(), Value::Int(3)),
            ("flag".into(), Value::Bool(false)),
        ],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "c",
        vec![("status".into(), Value::Str("draft".into()))],
    )
    .unwrap();

    // Untagged strings in an `in` list evaluate as the tagged ones would.
    assert_eq!(mask_keys(&db, "reader"), vec!["a", "b"]);
    // Untagged `eq` string.
    assert_eq!(mask_keys(&db, "core"), vec!["a"]);
    // One list, both spellings: 2 plain, {"Int": 3} tagged.
    assert_eq!(mask_keys(&db, "tier"), vec!["a", "b"]);
    // Untagged boolean.
    assert_eq!(mask_keys(&db, "flagged"), vec!["a"]);

    // The parsed predicate is the same value either way.
    let roles = db.roles();
    let reader = roles.iter().find(|r| r.name == "reader").unwrap();
    assert_eq!(
        reader.visible_where.as_ref().unwrap(),
        &PropPredicate {
            field: "status".into(),
            eq: None,
            in_: Some(vec![
                Value::Str("published".into()),
                Value::Str("archived".into())
            ]),
        }
    );

    // Re-applying rewrites the sidecar in the tagged form, and it still resolves
    // identically — the two spellings are one predicate.
    db.apply_schema(&roles_schema(vec![reader.clone()]))
        .unwrap();
    assert_eq!(mask_keys(&db, "reader"), vec!["a", "b"]);
}

/// A predicate value that is neither a scalar nor a tagged value is refused,
/// and refusing means poisoning — never silently dropping the narrowing.
#[test]
fn visible_where_refuses_a_value_it_cannot_read() {
    let dir = tmp("predicate-bad-value");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("roles.json"),
        br#"{"version":3,"roles":[{"name":"reader","keys":[],"labels":["Document"],
             "visible_where":{"field":"status","eq":{"not":"a value"}}}]}"#,
    )
    .unwrap();

    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.mask_for_role("reader").is_err(),
        "an unreadable predicate value must poison, not resolve to the whole label"
    );
}

/// The spec's own schema JSON applies as written — it is what a user copies.
#[test]
fn the_spec_schema_snippet_applies_as_written() {
    let dir = tmp("predicate-spec-snippet");
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();

    let schema: Schema = serde_json::from_str(
        r#"{ "roles": [
              { "name": "reader",
                "labels": ["Document", "Note"],
                "visible_where": { "field": "status", "in": ["published", "archived"] } },
              { "name": "editor",
                "labels": ["Document"],
                "visible_where": { "field": "workspace", "eq": "core" } }
            ] }"#,
    )
    .expect("the spec's schema JSON must deserialize");
    db.apply_schema(&schema).expect("and apply");

    db.insert_node(
        "Document",
        "d1",
        vec![
            ("status".into(), Value::Str("published".into())),
            ("workspace".into(), Value::Str("core".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "Note",
        "n1",
        vec![("status".into(), Value::Str("archived".into()))],
    )
    .unwrap();
    db.insert_node(
        "Document",
        "d2",
        vec![
            ("status".into(), Value::Str("draft".into())),
            ("workspace".into(), Value::Str("side".into())),
        ],
    )
    .unwrap();

    assert_eq!(mask_keys(&db, "reader"), vec!["d1", "n1"]);
    assert_eq!(mask_keys(&db, "editor"), vec!["d1"]);
}
