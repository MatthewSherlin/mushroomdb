//! The first minute, against the shipped binary.
//!
//! Every assertion here is a flow a new user takes on their first store, and
//! every one of them failed on v0.6.12:
//!
//!   upsert_entity -> remember -> recall            returned nothing
//!   remember{about:[unknown]}                      errored
//!   recall{topic:"Matthew"}                        returned nothing, though
//!                                                  the note was indexed and
//!                                                  contained the word
//!
//! It spawns `mushroomdb mcp <dir>` rather than calling into the library, so a
//! regression in surface selection, tool listing or install wiring fails here
//! too. It must never assert against a list this file declares: the 0.6.12
//! version of that idea passed with its bug reintroduced.
use core_api::GraphDb;
use serde_json::Value as Js;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mdb-firstrun-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ))
}

/// Drive `mushroomdb mcp <dir>` with one line per request; return the parsed
/// responses keyed by id.
fn mcp(dir: &PathBuf, calls: &[Js]) -> std::collections::BTreeMap<u64, Js> {
    let mut lines = vec![
        serde_json::json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2024-11-05","capabilities":{},
            "clientInfo":{"name":"first-run","version":"1"}}})
        .to_string(),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
    ];
    lines.extend(calls.iter().map(|c| c.to_string()));

    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("mcp")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mushroomdb mcp");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{}\n", lines.join("\n")).as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("mcp exited");

    let mut by_id = std::collections::BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Js = serde_json::from_str(line).expect("each stdout line is one JSON object");
        if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
            by_id.insert(id, v);
        }
    }
    by_id
}

fn text_of(resp: &Js) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn is_error(resp: &Js) -> bool {
    resp.get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
}

#[test]
fn upsert_then_remember_then_recall_returns_the_fact() {
    let dir = tmp("roundtrip");
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"upsert_entity","arguments":{
                    "key":"matthew","label":"Person",
                    "props":{"name":"Matthew Sherlin","role":"founder"}}}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"remember","arguments":{
                    "text":"Matthew prefers concise scannable summaries over prose",
                    "about":["matthew"]}}}),
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"recall","arguments":{"topic":"Matthew"}}}),
        ],
    );
    assert!(
        !is_error(&r[&1]),
        "upsert_entity failed: {}",
        text_of(&r[&1])
    );
    assert!(!is_error(&r[&2]), "remember failed: {}", text_of(&r[&2]));
    let recalled = text_of(&r[&3]);
    assert!(
        recalled.contains("concise"),
        "recall did not return the remembered fact.\ngot: {recalled}"
    );
}

#[test]
fn recall_answers_a_natural_question() {
    let dir = tmp("natural");
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"remember","arguments":{
                    "text":"The 0.7 release focuses on the memory write path"}}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"recall","arguments":{"topic":"What does 0.7 focus on?"}}}),
        ],
    );
    let recalled = text_of(&r[&2]);
    assert!(
        recalled.contains("write path"),
        "a natural-language topic returned nothing.\ngot: {recalled}"
    );
}

#[test]
fn remembering_about_an_unknown_subject_succeeds() {
    let dir = tmp("provisional");
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"remember","arguments":{
                    "text":"Reid reviewed the launch copy","about":["reid"]}}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"query","arguments":{
                    // `WHERE n.provisional` (truthy), not `= true`: the
                    // engine's Cypher has no boolean-literal comparison (an
                    // unrelated, pre-existing gap — `n.flag = true` is an
                    // "unbound variable `true`" error), and a standalone
                    // boolean predicate is the documented way to ask this
                    // (`core-query/src/cypher/parser.rs`, `Expr::Truthy`).
                    "cypher":"MATCH (n) WHERE n.provisional RETURN count(n) AS c"}}}),
        ],
    );
    assert!(
        !is_error(&r[&1]),
        "remember about an unknown subject errored: {}",
        text_of(&r[&1])
    );
    let counted: Js = serde_json::from_str(&text_of(&r[&2])).expect("query reply is JSON");
    assert_eq!(
        counted["rows"],
        serde_json::json!([[1]]),
        "exactly one provisional node must exist.\ngot: {counted}"
    );
}

#[test]
fn a_store_with_no_text_index_says_so_rather_than_saying_no_match() {
    // A *brand-new* store gets the memory schema for free (see
    // `an_mcp_created_store_can_find_the_entity_it_was_told_about`), so this
    // scenario can no longer be reached by pointing `mcp` at an empty
    // directory. It is still real: a store built through some other path
    // that declares no schema of its own — e.g. a pre-0.7 store, or any
    // store `mcp` did not create — and existing stores are never silently
    // upgraded (ledger row 36: declaring full-text on a populated store
    // rebuilds the index at open, 227 ms vs. 3.8 ms with none). Write
    // one node directly through core-api, bypassing both `demo` and `mcp`, so
    // the directory already holds a store — and no schema — before `mcp`
    // ever opens it.
    let dir = tmp("noindex");
    {
        let mut db = GraphDb::open(&dir).expect("create a store directly, no schema applied");
        db.insert_node("Widget", "w1", vec![]).expect("insert");
    }
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"recall","arguments":{"topic":"anything at all"}}}),
        ],
    );
    let out = text_of(&r[&1]);
    assert!(
        out.contains("schema apply"),
        "a store with no text index must name the fix.\ngot: {out}"
    );
}

#[test]
fn an_mcp_created_store_can_find_the_entity_it_was_told_about() {
    // The gate this whole task exists to close: a fresh `mcp <dir>` store
    // must be able to find the *entity* `upsert_entity` created, by its own
    // name — not merely the note text `remember` happens to self-index. On
    // 0.6.12 (and on this branch before the memory schema was applied at
    // `mcp`-store creation) `Note.text` was the only field ever indexed, so a
    // recall for the entity's own name found nothing.
    let dir = tmp("entity-recall");
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"upsert_entity","arguments":{
                    "key":"matthew","label":"Person",
                    "props":{"name":"Matthew Sherlin","role":"founder"}}}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"recall","arguments":{"topic":"Matthew Sherlin"}}}),
        ],
    );
    assert!(
        !is_error(&r[&1]),
        "upsert_entity failed: {}",
        text_of(&r[&1])
    );
    let recalled = text_of(&r[&2]);
    assert!(
        recalled.contains("matthew"),
        "recall for the entity's own name did not return the entity's key.\ngot: {recalled}"
    );
}

/// The README's quick start, run as printed: remember → recall →
/// suggest_rules → (create_rule, on approval) → explain_association.
///
/// The README may only print a flow this test proves. A dozen people in two
/// teams is the smallest store the flow is claimed for: `suggest_rules` needs
/// enough nodes sharing a value to propose a rule over it.
#[test]
fn the_readme_quick_start_runs_as_printed() {
    let dir = tmp("quickstart");

    // remember ×12 — each a sentence and the entity it names, with a team.
    let mut calls: Vec<Js> = (0..12u64)
        .map(|i| {
            let team = ["infra", "ui"][(i % 2) as usize];
            serde_json::json!({"jsonrpc":"2.0","id":i + 1,"method":"tools/call","params":{
                "name":"remember","arguments":{
                    "text": format!("Person {i} joined the {team} team"),
                    "ts": 1_759_000_000u64 + i,
                    "entities":[{"key": format!("p{i}"), "label":"Person",
                                 "props":{"name": format!("Person {i}"), "team": team}}]}}})
        })
        .collect();
    // recall, in ordinary words.
    calls.push(
        serde_json::json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{
        "name":"recall","arguments":{"topic":"who joined the infra team"}}}),
    );
    // suggest_rules, as data, so the proposal's arguments can be passed on.
    calls.push(
        serde_json::json!({"jsonrpc":"2.0","id":21,"method":"tools/call","params":{
        "name":"suggest_rules","arguments":{"json":true}}}),
    );
    let r = mcp(&dir, &calls);

    for id in 1..=12u64 {
        assert!(
            !is_error(&r[&id]),
            "remember {id} failed: {}",
            text_of(&r[&id])
        );
    }
    let recalled = text_of(&r[&20]);
    assert!(
        recalled.contains("note:") && recalled.contains("infra"),
        "recall did not return a remembered fact: {recalled}"
    );

    let report: Js = serde_json::from_str(&text_of(&r[&21])).expect("suggest_rules json");
    let over_team = report["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .find(|s| {
            s["create_rule_args"]
                .to_string()
                .contains("\"field\":\"team\"")
        })
        .unwrap_or_else(|| panic!("no proposal over `team` on a twelve-person store: {report}"));
    let args = over_team["create_rule_args"].clone();
    let rule_name = args["name"].as_str().expect("a rule name").to_string();

    // create_rule with the proposal's arguments, unchanged; then the question
    // the product exists for.
    let r = mcp(
        &dir,
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"create_rule","arguments": args}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"explain_association","arguments":{"a":"p0","b":"p2"}}}),
        ],
    );
    assert!(
        !is_error(&r[&1]),
        "create_rule refused its own suggestion: {}",
        text_of(&r[&1])
    );
    let why = text_of(&r[&2]);
    assert!(
        why.contains(&rule_name) && why.contains("team"),
        "explain_association did not name the rule and the field: {why}"
    );
}
