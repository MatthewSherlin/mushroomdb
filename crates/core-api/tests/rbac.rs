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

use core_api::schema::Schema;
use core_api::{GraphDb, RoleDef, Value};
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
        rules: vec![],
        views: vec![],
        roles: vec![analyst_role()],
    };
    db.apply_schema(&schema_v1).unwrap();

    let mut changed = analyst_role();
    changed.keys = vec!["bob".into()]; // different from original

    let schema_v2 = Schema {
        fulltext: vec![],
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
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "viewer".into(),
            keys: vec!["alice".into()],
            labels: vec!["Public".into()],
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
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "viewer".into(),
            keys: vec![],
            labels: vec!["Public".into()],
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
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "nothing".into(),
            keys: vec![],
            labels: vec![],
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
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "".into(),
            keys: vec![],
            labels: vec![],
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
        rules: vec![],
        views: vec![],
        roles: vec![
            RoleDef {
                name: "viewer".into(),
                keys: vec![],
                labels: vec![],
            },
            RoleDef {
                name: "viewer".into(),
                keys: vec!["alice".into()],
                labels: vec![],
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
