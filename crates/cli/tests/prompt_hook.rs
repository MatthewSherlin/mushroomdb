//! The `UserPromptSubmit` hook, driven the way Claude Code drives it.
//!
//! The hook is the only automatic path to `recall`, and until 0.7 plan 2 it
//! refused every prompt that did not name a path, a `mod::name`, a
//! snake_case word or something in backticks — so on a memory store, whose
//! prompts are sentences about people and projects, it was silent always.
//! This suite drives the real binary with a real payload on stdin.
use std::io::Write;
use std::process::{Command, Stdio};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-hook-{name}-{}-{nanos}", std::process::id()))
}

/// Run `mushroomdb recall <db>` with `payload` on stdin, as the host does.
fn hook(db: &std::path::Path, payload: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("recall")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn recall");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "hook must never fail a prompt: {out:?}"
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Run `mushroomdb brief <db>`, as the `SessionStart` hook does.
fn brief(db: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("brief")
        .arg(db)
        .output()
        .expect("run brief");
    assert!(
        out.status.success(),
        "brief must never fail a session: {out:?}"
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A memory store written through the real binary, so the schema the `mcp`
/// and `serve` paths apply is the schema under test here too.
fn store(name: &str) -> std::path::PathBuf {
    let db = tmp(name);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
            .args(args)
            .output()
            .expect("run");
        assert!(out.status.success(), "{args:?} failed: {out:?}");
    };
    let d = db.to_string_lossy().to_string();
    run(&["schema", "apply", &d, "--memory-defaults"]);
    run(&[
        "query",
        &d,
        "CREATE (n:Person {id:'matthew', name:'Matthew Sherlin'})",
    ]);
    run(&[
        "query",
        &d,
        "CREATE (n:Note {id:'note-1', text:'Matthew prefers concise summaries'})",
    ]);
    db
}

/// A store holding one node and no schema at all — the shape a 0.6.x store
/// has after upgrading to 0.7, before anyone runs `schema apply`.
fn store_without_an_index(name: &str) -> std::path::PathBuf {
    let db = tmp(name);
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args(["query", &db.to_string_lossy(), "CREATE (n:Person {id:'x'})"])
        .output()
        .expect("seed");
    assert!(out.status.success(), "{out:?}");
    db
}

#[test]
fn a_natural_language_prompt_gets_a_digest() {
    let db = store("natural");
    let out = hook(&db, r#"{"prompt":"What does Matthew prefer?"}"#);
    assert!(out.contains("note-1"), "the fact was not recalled: {out:?}");
    assert!(
        out.contains("matthew"),
        "the subject was not recalled: {out:?}"
    );
}

#[test]
fn a_prompt_about_nothing_in_the_store_stays_silent() {
    let db = store("silent");
    let out = hook(&db, r#"{"prompt":"what is the weather in Reykjavik"}"#);
    assert!(
        out.trim().is_empty(),
        "a hook with nothing to say says nothing: {out:?}"
    );
}

#[test]
fn a_malformed_payload_is_silent_and_successful() {
    // A recall hook must never block or fail a user's prompt.
    let db = store("malformed");
    assert!(hook(&db, "not json at all").trim().is_empty());
    assert!(hook(&db, "{}").trim().is_empty());
}

#[test]
fn a_store_with_no_text_index_leaves_the_prompt_alone() {
    // Deliberately *not* the MCP tool's behaviour. `recall` the tool answers
    // "this store has no text index, run schema apply", because someone asked
    // it a question and is owed the reason. This hook was not asked: it fires
    // on every prompt, so the same message here would nag on every turn until
    // the store is fixed. The notice belongs where it is paid for once — the
    // `SessionStart` brief — and `crates/cli/tests/recall.rs`'s
    // `recall_is_silent_when_no_fulltext_index_is_enabled` already holds this
    // and keeps passing.
    let db = store_without_an_index("noindex");
    assert!(hook(&db, r#"{"prompt":"who is x"}"#).trim().is_empty());
}

/// Binding: the session brief is where a store that cannot be searched says
/// so — once, with the command that fixes it — because the prompt hook above
/// stays silent on it every turn.
#[test]
fn the_brief_of_a_store_with_no_text_index_names_the_fix() {
    let db = store_without_an_index("brief-noindex");
    let out = brief(&db);
    let expected = format!(
        "no text index: recall cannot match anything here. \
         Run: mushroomdb schema apply '{}' --memory-defaults",
        db.display()
    );
    let lines: Vec<&str> = out.lines().collect();
    // Above the reach line, which stays the brief's last.
    assert_eq!(
        lines.len().checked_sub(2).map(|i| lines[i]),
        Some(expected.as_str()),
        "the notice sits just above the reach line: {out:?}"
    );
    assert!(
        lines
            .last()
            .is_some_and(|l| l.starts_with("reach the graph: ")),
        "{out:?}"
    );
    assert_eq!(out.matches("no text index").count(), 1, "{out:?}");
}

/// The notice names the store the way the reach line under it does: shell
/// quoted, so a path with a space or a quote in it is still one argument, and
/// sanitized, so a control character in the path cannot forge a line of the
/// brief.
#[test]
fn the_no_index_notice_quotes_and_sanitizes_the_store_path() {
    let db = store_without_an_index("brief noindex 'q'\u{1b}[31m");
    let out = brief(&db);
    let quoted =
        format!("'{}'", db.to_string_lossy().replace('\'', r"'\''")).replace('\u{1b}', " ");
    let expected = format!(
        "no text index: recall cannot match anything here. \
         Run: mushroomdb schema apply {quoted} --memory-defaults"
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len().checked_sub(2).map(|i| lines[i]),
        Some(expected.as_str()),
        "{out:?}"
    );
    assert!(!out.contains('\u{1b}'), "{out:?}");
}

#[test]
fn the_brief_of_a_searchable_store_says_nothing_about_an_index() {
    let db = store("brief-indexed");
    let out = brief(&db);
    assert!(!out.trim().is_empty(), "expected a brief: {out:?}");
    assert!(
        !out.contains("schema apply") && !out.contains("no text index"),
        "a store that can be searched needs no notice: {out:?}"
    );
}

/// One `remember` through `mushroomdb mcp <db>`, the way a session writes one.
fn remember_over_mcp(db: &std::path::Path, text: &str, about: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("mcp")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    let requests = format!(
        concat!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"prompt-hook-test","version":"0"}}}}}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#,
            "\n",
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"remember","arguments":{{"text":"{text}","about":["{about}"]}}}}}}"#,
            "\n",
        ),
        text = text,
        about = about,
    );
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(requests.as_bytes())
        .expect("write requests");
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("remembered note:"),
        "remember failed: {out:?}"
    );
}

/// A store that never took the memory defaults, holding one named `Person`,
/// after its first `remember`: `Note.text` is declared and nothing else is.
/// A 0.6.x store that has remembered anything has this shape.
fn store_past_its_first_note(name: &str) -> std::path::PathBuf {
    let db = tmp(name);
    let out = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args([
            "query",
            &db.to_string_lossy(),
            "CREATE (n:Person {id:'p1', name:'Pat Doe'})",
        ])
        .output()
        .expect("seed");
    assert!(out.status.success(), "{out:?}");
    remember_over_mcp(&db, "Pat Doe likes tea", "p1");
    db
}

/// Binding: the brief keeps naming the fix after the first `remember` gave the
/// store one text field (ledger row 73). `remember` declares `Note.text`
/// itself, so "no text index" stops being true at the first note, while the
/// person already in the store is still not found by name.
#[test]
fn the_brief_of_a_store_past_its_first_note_names_the_fields_recall_cannot_search() {
    let db = store_past_its_first_note("brief-incomplete");
    let out = brief(&db);
    let expected = format!(
        "text index incomplete: recall does not search Person.name. \
         Run: mushroomdb schema apply '{}' --memory-defaults",
        db.display()
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.len().checked_sub(2).map(|i| lines[i]),
        Some(expected.as_str()),
        "the notice sits just above the reach line: {out:?}"
    );
    assert!(!out.contains("no text index"), "{out:?}");
    assert!(
        out.len() <= core_api::memory::brief::MAX_BRIEF_BYTES,
        "an ordinary store's brief holds the notice inside the cap: {} bytes",
        out.len()
    );

    // The command it names ends it.
    let applied = Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .args([
            "schema",
            "apply",
            &db.to_string_lossy(),
            "--memory-defaults",
        ])
        .output()
        .expect("schema apply");
    assert!(applied.status.success(), "{applied:?}");
    let after = brief(&db);
    assert!(
        !after.contains("schema apply") && !after.contains("text index"),
        "{after:?}"
    );
}

/// The prompt hook fires every turn, so it carries no notice — only the
/// digest. The brief says it once.
#[test]
fn the_prompt_hook_says_nothing_about_an_incomplete_index() {
    let db = store_past_its_first_note("hook-incomplete");
    let out = hook(&db, r#"{"prompt":"pat doe"}"#);
    assert!(out.contains("Pat Doe likes tea"), "{out:?}");
    assert!(
        !out.contains("text index") && !out.contains("schema apply"),
        "{out:?}"
    );
}
