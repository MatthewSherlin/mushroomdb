//! § 6.2 Adversarial checklist — one test per checkbox item.
//!
//! These tests are the phase gate: every §6.2 item has a named passing test.
//! Any finding in a checked item is Critical and blocks merge.
//!
//! §6.2 item → test name mapping:
//!   1.  Hidden-node probe via MERGE          → hidden_node_probe_via_merge
//!   2.  Label-scope overflow                 → label_scope_overflow
//!   3.  Edge with hidden endpoint            → edge_with_hidden_endpoint
//!   4.  Rule-tripped hidden edge not revealed → rule_tripped_hidden_edge_not_revealed
//!   5.  MERGE visibility oracle (structural)  → merge_visibility_oracle
//!   6.  Write-scope + read-mask orthogonality → write_scope_read_mask_orthogonality
//!   7.  Concurrent writer interference       → concurrent_writer_interference
//!   8.  apply_schema / POST /rules with role → apply_schema_with_role_token
//!   9.  V1 sidecar loaded by v0.3 server     → v1_sidecar_loaded_by_v03_server
//!   10. V2 sidecar on hypothetical v0.2 server → v2_sidecar_on_v02_server

#![allow(deprecated)]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use core_api::{schema::Schema, BatchOp, Predicate, RoleDef, RuleDef, SharedDb, Value, WriteScope};
use serde_json::{json, Value as Json};
use server::router_with_role_tokens;
use std::path::PathBuf;
use tower::ServiceExt;

// ── helpers ──────────────────────────────────────────────────────────────────

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-adv-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Open a DB, apply a schema (roles + optional extra rules), and wrap in a
/// `router_with_role_tokens` app.
///
/// `roles`:          (role_name, read_labels, write_scope)
/// `extra_rules`:    additional RuleDef declarations
/// `role_token_map`: (bearer_value, role_name)
fn open_adv(
    name: &str,
    roles: &[(&str, &[&str], Option<WriteScope>)],
    extra_rules: Vec<RuleDef>,
    full_token: Option<&str>,
    role_token_map: &[(&str, &str)],
) -> (Router, SharedDb) {
    let db = SharedDb::open(&tmp(name)).unwrap();
    let schema = Schema {
        roles: roles
            .iter()
            .map(|(rname, labels, write)| RoleDef {
                name: rname.to_string(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
                keys: vec![],
                write: write.clone(),
            })
            .collect(),
        rules: extra_rules,
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();
    let rtoks: std::collections::HashMap<String, String> = role_token_map
        .iter()
        .map(|(tok, role)| (tok.to_string(), role.to_string()))
        .collect();
    let app = router_with_role_tokens(db.clone(), full_token.map(str::to_string), rtoks);
    (app, db)
}

async fn send(app: Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let body = to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

fn parse_json(bytes: &[u8]) -> Json {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("json: {e}: {}", String::from_utf8_lossy(bytes)))
}

fn authed_json_req(method: &str, uri: &str, token: &str, body: Json) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn authed_get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ── §6.2 item 1: Hidden-node probe via MERGE ─────────────────────────────────
//
// Attack: A role token sends MERGE on a key that belongs to a hidden node.
// If the server returns a different response than for a non-existent key, the
// role has an existence oracle for hidden nodes.
//
// Closure: An update-only role (create_labels empty) has no create arm for
// MERGE. Under the decision table, MERGE on any absent key — whether the key
// is genuinely absent or is hidden under a different label — returns 403
// "target node not visible" (spec §3.3). The two response bodies are pinned
// byte-equal here at the HTTP level. Engine-level pin: test_merge_update_only_
// hidden_eq_absent in crates/core-api/tests/write_scopes.rs.
#[tokio::test]
async fn hidden_node_probe_via_merge() {
    let (app, db) = open_adv(
        "adv1-merge-probe",
        &[(
            "updater",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec![],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        vec![],
        Some("admin"),
        &[("rtok", "updater")],
    );

    // Admin inserts a hidden node: label "Secret" is outside the role's read labels.
    db.write()
        .insert_node("Secret", "probe-key", vec![])
        .unwrap();

    // Role MERGE on the hidden key ("probe-key" exists in DB, hidden from role).
    let (st_hidden, body_hidden) = send(
        app.clone(),
        authed_json_req(
            "POST",
            "/query",
            "rtok",
            json!({"cypher": "MERGE (n:AgentNote {id: 'probe-key'})"}),
        ),
    )
    .await;

    // Role MERGE on a key that does not exist at all.
    let (st_absent, body_absent) = send(
        app,
        authed_json_req(
            "POST",
            "/query",
            "rtok",
            json!({"cypher": "MERGE (n:AgentNote {id: 'nonexistent-key'})"}),
        ),
    )
    .await;

    assert_eq!(st_hidden, StatusCode::FORBIDDEN, "hidden MERGE must be 403");
    assert_eq!(st_absent, StatusCode::FORBIDDEN, "absent MERGE must be 403");

    // Existence oracle check: both responses must be byte-identical.
    assert_eq!(
        body_hidden, body_absent,
        "MERGE on hidden key and non-existent key must produce byte-identical 403 bodies"
    );

    let v = parse_json(&body_hidden);
    assert_eq!(
        v["error"].as_str().unwrap_or(""),
        "role-bound token: target node not visible",
        "§4.3 error body must be verbatim"
    );
}

// ── §6.2 item 2: Label-scope overflow ────────────────────────────────────────
//
// Attack: A role with create_labels: ["AgentNote"] sends CREATE (n:AdminLabel)
// — a label outside its scope. If allowed, the role can write arbitrary data
// under any label.
//
// Closure: Decision table row 1 (scope-before-lookup) fires: label not in
// create_labels → 403 scope-denied, no key lookup, no side effects.
#[tokio::test]
async fn label_scope_overflow() {
    let (app, _db) = open_adv(
        "adv2-label-overflow",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec!["AgentNote".into()],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        vec![],
        Some("admin"),
        &[("rtok", "agent")],
    );

    // CREATE under a label that is NOT in create_labels.
    let (status, body) = send(
        app,
        authed_json_req(
            "POST",
            "/nodes",
            "rtok",
            json!({"label": "AdminLabel", "key": "escalation", "props": {}}),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "out-of-scope CREATE must be 403"
    );
    let v = parse_json(&body);
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("AdminLabel") && msg.contains("create_labels"),
        "scope-denied body must name the label and the scope field: {msg}"
    );
}

// ── §6.2 item 3: Edge with hidden endpoint ────────────────────────────────────
//
// Attack: A role creates an edge where one endpoint is hidden. The write
// succeeds, confirming to the role that the hidden node exists (it can then
// inspect the visible node's /edges view after the write).
//
// Closure: Both edge endpoints must be in the role's current read mask at
// write time. Either hidden endpoint → 403 "edge endpoint not visible" (§3.4),
// checked inside the write lock before any WAL write.
#[tokio::test]
async fn edge_with_hidden_endpoint() {
    let (app, db) = open_adv(
        "adv3-edge-hidden-ep",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec![],
                create_edge_types: vec!["RECALLS".into()],
                delete_edge_types: vec![],
            }),
        )],
        vec![],
        Some("admin"),
        &[("rtok", "agent")],
    );

    {
        let mut w = db.write();
        // Source is visible (AgentNote in role's read labels).
        w.insert_node("AgentNote", "visible-src", vec![]).unwrap();
        // Destination is hidden (Secret label not in role's read labels).
        w.insert_node("Secret", "hidden-dst", vec![]).unwrap();
    }

    // POST /edges body uses "type" (not "edge_type") for the edge type field.
    let (status, body) = send(
        app,
        authed_json_req(
            "POST",
            "/edges",
            "rtok",
            json!({"type": "RECALLS", "src": "visible-src", "dst": "hidden-dst"}),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "edge with hidden endpoint must be 403"
    );
    let v = parse_json(&body);
    assert_eq!(
        v["error"].as_str().unwrap_or(""),
        "role-bound token: edge endpoint not visible",
        "§4.3 error body must be verbatim"
    );
}

// ── §6.2 item 4: Rule-tripped hidden edge not revealed ────────────────────────
//
// Attack: A role creates a node that triggers a derivation rule linking it to a
// hidden neighbor. The derived edge appears in /edges or /neighborhood, leaking
// the existence of the hidden node.
//
// Closure: Rule evaluation runs with DB authority (spec §3.5 — the rule engine
// is an integrity primitive, not an access-controlled policy layer). The derived
// edge is created. However, the role's read mask is applied at read time on
// every edge and neighborhood traversal: any edge whose endpoint is outside the
// role's label mask is filtered before the response is serialized. The derived
// edge to the hidden neighbor is never returned to the role, regardless of the
// rule firing.
//
// Confirmed leak-free: the admin token below verifies the derived edge DOES exist
// in the raw graph (the rule fired), proving the test is non-trivially exercising
// the mask-at-read-time closure, not a vacuously empty edge set.
#[tokio::test]
async fn rule_tripped_hidden_edge_not_revealed() {
    // Rule: AgentNote → Secret, LINKED when the "tag" field is equal.
    // When the role creates an AgentNote with tag="rust", and a hidden Secret
    // node already has tag="rust", this rule fires with DB authority and creates
    // a LINKED edge from the new AgentNote to the hidden Secret node.
    let link_rule = RuleDef {
        name: "link-to-secret".into(),
        src_label: "AgentNote".into(),
        dst_label: "Secret".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };

    let (app, db) = open_adv(
        "adv4-rule-hidden-edge",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        vec![link_rule],
        Some("admin"),
        &[("rtok", "agent")],
    );

    // Admin inserts a hidden neighbor with matching tag.
    db.write()
        .insert_node(
            "Secret",
            "secret-neighbor",
            vec![("tag".into(), Value::Str("rust".into()))],
        )
        .unwrap();

    // Role creates its own AgentNote with the same tag via the write-scoped path.
    // The rule fires (DB authority): a LINKED edge from "my-note" to "secret-neighbor"
    // is created inside commit_logged_batch before the response is sent.
    let (status, create_body) = send(
        app.clone(),
        authed_json_req(
            "POST",
            "/nodes",
            "rtok",
            json!({"label": "AgentNote", "key": "my-note", "props": {"tag": "rust"}}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "role create must succeed: {}",
        String::from_utf8_lossy(&create_body)
    );

    // Admin verification: confirm the derived LINKED edge EXISTS in the raw graph.
    // This proves the rule fired and the test is exercising a real hidden edge,
    // not an empty edge set.
    let (adm_st, adm_body) = send(app.clone(), authed_get("/node/my-note/edges", "admin")).await;
    assert_eq!(adm_st, StatusCode::OK, "admin /edges must be 200");
    let adm_v = parse_json(&adm_body);
    let adm_edges = adm_v["edges"].as_array().expect("admin edges array");
    let derived_exists = adm_edges.iter().any(|e| {
        e["dst_key"].as_str() == Some("secret-neighbor")
            || e["src_key"].as_str() == Some("secret-neighbor")
    });
    assert!(
        derived_exists,
        "admin must see the derived LINKED edge to secret-neighbor (confirms rule fired): {adm_edges:?}"
    );

    // Role verification: the LINKED edge must NOT appear in the role's /edges view.
    let (role_st, role_edges_body) =
        send(app.clone(), authed_get("/node/my-note/edges", "rtok")).await;
    assert_eq!(
        role_st,
        StatusCode::OK,
        "/edges must be 200 for visible node"
    );
    let rev = parse_json(&role_edges_body);
    let role_edges = rev["edges"].as_array().expect("role edges array");
    let hidden_present = role_edges.iter().any(|e| {
        e["src_key"].as_str() == Some("secret-neighbor")
            || e["dst_key"].as_str() == Some("secret-neighbor")
    });
    assert!(
        !hidden_present,
        "derived edge to hidden neighbor must not appear in role /edges: {role_edges:?}"
    );

    // Role /neighborhood must also exclude the hidden neighbor.
    let (nbhd_st, nbhd_body) = send(
        app,
        authed_get("/node/my-note/neighborhood?depth=2", "rtok"),
    )
    .await;
    assert_eq!(nbhd_st, StatusCode::OK, "/neighborhood must be 200");
    let nv = parse_json(&nbhd_body);
    let rows = nv["rows"].as_array().expect("neighborhood rows array");
    let hidden_in_rows = rows.iter().any(|r| {
        r.as_array()
            .and_then(|a| a.first())
            .and_then(|k| k.as_str())
            == Some("secret-neighbor")
    });
    assert!(
        !hidden_in_rows,
        "hidden neighbor must not appear in role /neighborhood: {rows:?}"
    );
}

// ── §6.2 item 5: MERGE visibility oracle ─────────────────────────────────────
//
// Threat: An adversary times MERGE 403 responses to distinguish a hidden key
// (DB lookup required) from a non-existent key (no lookup) — the latency
// difference leaks key existence.
//
// Why this is NOT measured as a statistical timing test:
//
// The closure is structural, not timing-based. The engine's decision table
// checks write scope (label ∈ create_labels OR update_labels) BEFORE consulting
// the key namespace. For a role that lacks scope for a label, no key lookup
// ever occurs — the scope-denied error is returned from the same code location
// regardless of key existence or hiddenness. The code path for "hidden key"
// and "absent key" is identical: both hit the scope check, both short-circuit,
// both produce the same error string.
//
// A statistical timing test would be unreliable (scheduler jitter, JIT, cache
// state) and is also unnecessary: the structural invariant is verified here by
// asserting byte-identical bodies, and is pinned at the engine level by
// test_merge_unscoped_no_key_lookup in crates/core-api/tests/write_scopes.rs,
// which confirms the scope denial fires even when the store is completely empty.
//
// Implementation: an unscoped role (create_labels/update_labels both empty for
// "AgentNote") attempts MERGE on (1) a key that exists but is hidden, and
// (2) a key that does not exist at all. Both return byte-identical 403 bodies.
#[tokio::test]
async fn merge_visibility_oracle() {
    let (app, db) = open_adv(
        "adv5-merge-oracle",
        &[(
            "reader",
            &["AgentNote"],
            Some(WriteScope {
                // No create or update scope for any label.
                // Decision table: MERGE returns scope-denied BEFORE any key lookup.
                create_labels: vec![],
                update_labels: vec![],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        )],
        vec![],
        Some("admin"),
        &[("rtok", "reader")],
    );

    // "probe-key" EXISTS in the DB under "Secret" — it is hidden from the role.
    db.write()
        .insert_node("Secret", "probe-key", vec![])
        .unwrap();

    // Role MERGE on the hidden key ("probe-key" exists, but role has no scope).
    // The scope check fires first — no key lookup occurs.
    let (st_hidden, body_hidden) = send(
        app.clone(),
        authed_json_req(
            "POST",
            "/query",
            "rtok",
            json!({"cypher": "MERGE (n:AgentNote {id: 'probe-key'})"}),
        ),
    )
    .await;

    // Role MERGE on a key that does not exist at all.
    let (st_absent, body_absent) = send(
        app,
        authed_json_req(
            "POST",
            "/query",
            "rtok",
            json!({"cypher": "MERGE (n:AgentNote {id: 'absent-key'})"}),
        ),
    )
    .await;

    assert_eq!(st_hidden, StatusCode::FORBIDDEN, "hidden MERGE must be 403");
    assert_eq!(st_absent, StatusCode::FORBIDDEN, "absent MERGE must be 403");

    // Structural pin: the code path is identical for both — scope check fires
    // before key lookup in both cases. Response bodies must be byte-identical.
    assert_eq!(
        body_hidden, body_absent,
        "scope-denied before key lookup: hidden and absent MERGE must return byte-identical bodies"
    );

    let v = parse_json(&body_hidden);
    assert_eq!(
        v["error"].as_str().unwrap_or(""),
        "role-bound token: label 'AgentNote' not in write scope (create_labels)",
        "§4.3 scope-denied body must be verbatim"
    );
}

// ── §6.2 item 6: Write-scope + read-mask orthogonality ───────────────────────
//
// Threat: A role declares create_labels for labels it cannot read (disjoint
// write and read scopes). Such a role would write into a blind spot, sidestepping
// the never-widen invariant.
//
// Closure (spec §7.1 ruling, controller 2026-08-28): create_labels,
// update_labels, and delete_labels must each be a subset of the role's read
// labels. apply_schema validates this at schema application time and returns an
// error naming the role and the offending label. Disjoint scopes are
// unrepresentable — the spec §7.1 alternative arm (require subset) is adopted.
#[test]
fn write_scope_read_mask_orthogonality() {
    let dir = tmp("adv6-scope-orthog");
    let db = SharedDb::open(&dir).unwrap();

    // "bad-role" declares create_labels: ["AgentNote"] but read labels: [].
    // create_labels ⊄ labels — must be rejected at apply_schema.
    let schema = Schema {
        roles: vec![RoleDef {
            name: "bad-role".into(),
            labels: vec![],
            keys: vec![],
            write: Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec![],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
        ..Default::default()
    };

    let result = db.write().apply_schema(&schema);
    assert!(
        result.is_err(),
        "apply_schema must REJECT a role where create_labels ⊄ read labels"
    );
    let err_msg = result.unwrap_err().to_string();
    // The error must identify the offending role and/or label.
    assert!(
        err_msg.contains("bad-role") || err_msg.contains("AgentNote"),
        "error must name the role or offending label: {err_msg}"
    );
}

// ── §6.2 item 7: Concurrent writer interference ───────────────────────────────
//
// Threat: Two role tokens writing to the same node concurrently observe a
// partial intermediate state if writes are not fully serialized.
//
// Closure: All write-scoped submissions go through SharedDb::submit_batch_authz,
// which enqueues them through a single drain queue. The drain loop holds the
// write lock (inner.write()) for the duration of each batch commit. Both
// submissions serialize, each batch is atomic (all-or-nothing WAL frame), and
// neither sees a partial intermediate state.
//
// The test asserts both SetProp operations succeed and both properties are
// durably present on the node after both threads join — no partial state.
#[test]
fn concurrent_writer_interference() {
    let dir = tmp("adv7-concurrent-write");
    let db = SharedDb::open(&dir).unwrap();
    let schema = Schema {
        roles: vec![RoleDef {
            name: "agent".into(),
            labels: vec!["AgentNote".into()],
            keys: vec![],
            write: Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec![],
                create_edge_types: vec![],
                delete_edge_types: vec![],
            }),
        }],
        ..Default::default()
    };
    db.write().apply_schema(&schema).unwrap();

    // Admin creates the shared target node.
    db.write()
        .insert_node("AgentNote", "shared-node", vec![])
        .unwrap();

    // Two threads each submit a SetProp on the same node simultaneously.
    // They compete for the drain queue — one is serialized before the other.
    let db1 = db.clone();
    let db2 = db.clone();
    let h1 = std::thread::spawn(move || {
        db1.submit_batch_authz(
            "agent".into(),
            vec![BatchOp::SetProp {
                key: "shared-node".into(),
                field: "from_t1".into(),
                value: Value::Int(1),
            }],
        )
    });
    let h2 = std::thread::spawn(move || {
        db2.submit_batch_authz(
            "agent".into(),
            vec![BatchOp::SetProp {
                key: "shared-node".into(),
                field: "from_t2".into(),
                value: Value::Int(2),
            }],
        )
    });

    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    assert!(r1.is_ok(), "concurrent write 1 must succeed: {r1:?}");
    assert!(r2.is_ok(), "concurrent write 2 must succeed: {r2:?}");

    // Both properties must be present — no write lost, no partial state.
    let r = db.read();
    assert_eq!(
        r.get_prop("shared-node", "from_t1"),
        Some(Value::Int(1)),
        "from_t1 must be durably set"
    );
    assert_eq!(
        r.get_prop("shared-node", "from_t2"),
        Some(Value::Int(2)),
        "from_t2 must be durably set"
    );
}

// ── §6.2 item 8: apply_schema / POST /rules with a role token ─────────────────
//
// Threat: A write-scoped role token sends POST /rules, attempting to declare a
// new derivation rule. A role that can inject rules can alter the rule engine's
// behavior for all tokens (including Full), potentially engineering hidden-node
// linkage that changes the graph structure outside the role's stated scope.
//
// Closure: POST /rules is in the permanent 403 list for all role tokens
// regardless of write scope (spec §4.2 — rule creation is schema-level
// administration, not a data write). The HTTP layer's role branch for POST /rules
// is unchanged and returns 403 "this endpoint is not permitted".
#[tokio::test]
async fn apply_schema_with_role_token() {
    // Role has the broadest possible write scope.
    let (app, _db) = open_adv(
        "adv8-rules-blocked",
        &[(
            "agent",
            &["AgentNote"],
            Some(WriteScope {
                create_labels: vec!["AgentNote".into()],
                update_labels: vec!["AgentNote".into()],
                delete_labels: vec!["AgentNote".into()],
                create_edge_types: vec!["RECALLS".into()],
                delete_edge_types: vec!["RECALLS".into()],
            }),
        )],
        vec![],
        Some("admin"),
        &[("rtok", "agent")],
    );

    // Role attempts to declare a rule that would link its visible nodes to hidden ones.
    let (status, body) = send(
        app,
        authed_json_req(
            "POST",
            "/rules",
            "rtok",
            json!({
                "name": "escalation",
                "src_label": "AgentNote",
                "dst_label": "Secret",
                "predicate": {"FieldEqual": {"field": "tag"}},
                "edge_type": "LEAKED"
            }),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "POST /rules with write-scoped role token must be 403: {}",
        String::from_utf8_lossy(&body)
    );
}

// ── §6.2 item 9: V1 sidecar loaded by v0.3 server ────────────────────────────
//
// Threat: The v0.3 server loads an existing v1 roles.json (no write fields,
// "version": 1) and grants write access to roles that were never granted it —
// breaking existing read-only deployments.
//
// Closure: A v1 sidecar has no write fields. serde's #[serde(default)] on
// write: Option<WriteScope> leaves it as None for every role. write: None is the
// v1 read-only path — exactly the behavior before v0.3. The server returns 403
// for all write attempts, unchanged from v1.
//
// Integration test: the roles.json is written as a literal v1 JSON file BEFORE
// SharedDb::open so the roles are loaded into the in-memory cache at open time.
// db.roles() returns the cached Vec<RoleDef>, not a live file read per request —
// same pattern as poisoned_sidecar_is_500_for_role_token in http.rs.
#[tokio::test]
async fn v1_sidecar_loaded_by_v03_server() {
    let dir = tmp("adv9-v1-sidecar");

    // Create the dir and write a literal v1-format roles.json BEFORE open.
    std::fs::create_dir_all(&dir).unwrap();
    let v1_json = r#"{"version":1,"roles":[{"name":"reader","labels":["AgentNote"],"keys":[]}]}"#;
    std::fs::write(dir.join("roles.json"), v1_json).unwrap();

    // Open: roles.json is read and cached. Role "reader" has write: None.
    let db = SharedDb::open(&dir).unwrap();

    let rtoks: std::collections::HashMap<String, String> =
        [("vtok".to_string(), "reader".to_string())]
            .into_iter()
            .collect();
    let app = router_with_role_tokens(db, Some("admin".into()), rtoks);

    // A write attempt from the v1 role token must be 403 — same as v1 behavior.
    let (status, body) = send(
        app,
        authed_json_req(
            "POST",
            "/nodes",
            "vtok",
            json!({"label": "AgentNote", "key": "attempt", "props": {}}),
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "v1 sidecar role must be read-only (403 on write attempt): {}",
        String::from_utf8_lossy(&body)
    );
}

// ── §6.2 item 10: V2 sidecar on a hypothetical v0.2 server ───────────────────
//
// Documented test (spec §6.2 / §2): A v2 roles.json carrying write fields is
// loaded by a hypothetical v0.2 server. serde's #[serde(default)] on
// write: Option<WriteScope> means the unknown "write" field is silently ignored.
// All roles parse as write: None (read-only). The v0.2 server denies all writes
// from role tokens anyway (blanket 403), so the net behavior is safe and
// forward-compatible without any migration or version-gate code.
//
// Implementation: a v0.2-shaped RoleDef struct (no `write` field) is used to
// deserialize a v2 JSON. serde must accept the JSON and drop the unknown field.
// The parsed roles behave as read-only because the write scope is absent from
// the struct.

#[derive(serde::Deserialize, Debug, PartialEq)]
struct RoleDefV0 {
    name: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    keys: Vec<String>,
    // No `write` field: a v0.2 server's struct shape.
    // serde silently drops the "write" key from v2 JSON.
}

#[derive(serde::Deserialize, Debug)]
struct RolesFileV0 {
    version: u32,
    roles: Vec<RoleDefV0>,
}

#[test]
fn v2_sidecar_on_v02_server() {
    // A v2 roles.json with a fully-specified write scope (spec §2 example).
    let v2_json = r#"{
        "version": 2,
        "roles": [
            {
                "name": "agent-memory",
                "labels": ["AgentNote", "AgentContext"],
                "keys": [],
                "write": {
                    "create_labels": ["AgentNote", "AgentContext"],
                    "update_labels": ["AgentNote"],
                    "delete_labels": ["AgentNote"],
                    "create_edge_types": ["RECALLS"],
                    "delete_edge_types": ["RECALLS"]
                }
            }
        ]
    }"#;

    // A v0.2-shaped parser must successfully deserialize the v2 JSON.
    // serde's unknown-field handling drops the "write" key silently.
    let parsed: RolesFileV0 = serde_json::from_str(v2_json)
        .expect("v0.2-shaped parser must accept v2 sidecar (unknown fields dropped by serde)");

    assert_eq!(parsed.version, 2, "version field preserved");
    assert_eq!(parsed.roles.len(), 1, "role count correct");

    let role = &parsed.roles[0];
    assert_eq!(role.name, "agent-memory");
    assert_eq!(role.labels, vec!["AgentNote", "AgentContext"]);

    // The `write` field is absent from RoleDefV0 — serde dropped it.
    // A v0.2 server holding this struct treats the role as read-only
    // (equivalent to write: None) and denies all write requests (blanket 403).
    // Net behavior: forward-compatible, safe, no migration needed.
    //
    // No further assertion needed: the absence of `write` from RoleDefV0 is
    // itself the structural proof that the field was dropped at parse time.
}
