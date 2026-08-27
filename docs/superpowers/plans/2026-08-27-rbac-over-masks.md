# RBAC over masks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Named roles, declared in schema-as-code, bound to server tokens, resolved automatically to node masks on every read — role-based visibility with zero per-query work by the client.

**Architecture:** Roles layer on two shipped primitives: node masks (visibility) and `apply_schema` (declaration). A role = an allow-list built from explicit node keys and/or label selectors, materialized to a mask at query time (label selectors resolve live, so new nodes of an allowed label are visible without re-declaration). Persistence v1 is a `roles.json` SIDECAR in the db dir (atomic write, loaded at open) — deliberately NOT a WAL/snapshot format change, to avoid colliding with the in-flight Phase B V8 work; in-WAL roles are a v0.3 follow-up once V8 lands. Server: tokens bind to roles; role-bound tokens get masked reads and are DENIED writes and subscriptions (v1 safe default — no event-leak surface).

**Tech Stack:** In-tree Rust only, no new crates.

**Spec:** `docs/superpowers/specs/2026-08-27-v0.2-association-engine.md` §5 Addendum (RBAC over masks). Runs PARALLEL to Phase B on its own branch; server/http.rs overlap with Phase B Task 4 is a known merge point (ledgered).

## Global Constraints

- Full-access behavior unchanged: no role config → exactly today's semantics, byte-identical.
- Role-bound tokens: masked reads on `/query` + read endpoints; 403 on writes, /subscribe, /watch, /ingest, /rules POST (clear error body naming the reason). The full token retains everything.
- Role resolution NEVER widens: unknown role on a token = 401 at request time; empty role = sees nothing.
- roles.json writes are atomic (temp+rename+fsync, mirror write_atomic semantics) and only via `apply_schema`/CLI — never hand-edited state the code trusts blindly (validate on load; on corrupt roles.json, FAIL OPEN-loudly: server refuses role-token requests with 500 until fixed, full token unaffected — never silently grant wider visibility).
- Workspace suite + fmt + clippy clean per task. Conventional lowercase commits, no Co-Authored-By.

---

### Task 1: RoleDef, Schema.roles, sidecar persistence, mask resolution

**Files:** `crates/core-api/src/schema.rs` (RoleDef + Schema.roles + apply), new `crates/core-api/src/roles.rs` (sidecar load/store + resolution), `crates/core-api/src/lib.rs` exports, tests `crates/core-api/tests/rbac.rs` (new), `crates/cli/src/lib.rs` (`schema apply` picks up roles automatically).

**Interfaces:**

```rust
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct RoleDef {
    pub name: String,
    /// Explicit node keys always visible to the role.
    #[serde(default)] pub keys: Vec<String>,
    /// All nodes carrying any of these labels are visible (resolved live).
    #[serde(default)] pub labels: Vec<String>,
}
// Schema gains: #[serde(default)] pub roles: Vec<RoleDef>
impl GraphDb<F> {
    /// Resolve a role to a mask against CURRENT data. Err if role unknown.
    pub fn mask_for_role(&self, role: &str) -> Result<NodeMask>;
    pub fn roles(&self) -> Vec<RoleDef>;
}
```

Bindings: apply_schema diff entries `role:NAME` (created/updated/unchanged via PartialEq, same pre-validation discipline: role names non-empty + unique in schema); roles.json = `{ "version": 1, "roles": [...] }` written atomically on apply, loaded at open into memory; `mask_for_role` = keys resolved via ids (unknown keys ignored, consistent with NodeMask::from_keys) UNION live label scan; empty union = empty mask (sees nothing).

- [ ] **Step 1: failing tests** — apply schema with roles → diff has `role:analyst` created; re-apply → unchanged and roles.json byte-identical; changed role → updated; `mask_for_role`: keys+labels union correct, new node of allowed label visible WITHOUT re-apply, unknown role Err, empty role yields empty-visibility mask (query_masked returns 0 rows); corrupt roles.json → open succeeds but mask_for_role returns Err (fail-loud test).
- [ ] **Step 2: run to fail.** **Step 3: implement.** **Step 4: full suite.** **Step 5: commit** `feat: schema roles with sidecar persistence and mask resolution`

### Task 2: server role tokens — masked reads, denied writes

**Files:** `crates/server/src/http.rs` (token config + auth middleware + read-path mask application), `crates/cli/src/lib.rs`/`main.rs` (serve flags), `crates/server/tests/http.rs`, docs (`docs/site/api.md` auth section, README one paragraph).

**Interfaces:** serve gains repeatable `--role-token <TOKEN>:<ROLE>` (and env `MUSHROOMDB_ROLE_TOKENS="tok1:role1,tok2:role2"`); auth middleware resolves bearer → Full | Role(name); role requests: `/query` (reads) auto-apply `mask_for_role` (explicit client `mask` param INTERSECTS the role mask — never widens); GET node/neighborhood/stats/search endpoints masked the same way (node endpoints on hidden keys → 404-equivalent absent, matching mask semantics); writes + /ingest + /rules + /subscribe + /watch → 403 with `{"error":"role-bound token: read-only"}`-style body.

- [ ] **Step 1: failing tests** — role token sees only its subgraph via /query and /node; full token unaffected; write with role token → 403; unknown-role token → 401; client mask ∩ role mask semantics; /subscribe with role token → 403.
- [ ] **Steps 2-4: fail → implement → full suite.** **Step 5: commit** `feat: role-bound server tokens with masked reads`

---

## Gate

- All Task 1 + Task 2 tests green; workspace green; fmt+clippy clean.
- No-role-config path byte-identical (existing server tests untouched).
- docs: auth section documents roles, the read-only v1 contract, the sidecar location, and the never-widen intersection rule.
