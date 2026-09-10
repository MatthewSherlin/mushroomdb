//! `mushroomdb intercept` — the optional `PreToolUse` hook body.
//!
//! It reads Claude Code's hook payload and decides whether the `Grep` about to
//! run is a question the graph already answers exactly: a bare identifier that
//! names a symbol. Anything that looks like a regex, anything the graph does
//! not hold, and anything malformed passes straight through, because a hook
//! that blocks a search wrongly is worse than one that never fires.

use cli::intercept::{decide, run_intercept};
use core_api::{GraphDb, Value};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Unique per call: tests run concurrently and two of them can read the same
/// nanosecond, which would otherwise hand both the same directory.
static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mushroomdb-intercept-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ))
}

/// A store holding two symbols: one whose name is long enough to redirect, one
/// too short for the identifier test to accept.
fn seed(name: &str) -> PathBuf {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).expect("open store");
    for (key, name) in [
        ("crates/cli/src/render.rs::render_map", "render_map"),
        ("crates/cli/src/render.rs::id", "id"),
    ] {
        db.insert_node(
            "Symbol",
            key,
            vec![("name".to_string(), Value::Str(name.to_string()))],
        )
        .expect("insert symbol");
    }
    dir
}

fn grep(pattern: &str) -> serde_json::Value {
    json!({"tool_name": "Grep", "tool_input": {"pattern": pattern}})
}

fn open(dir: &Path) -> cli::structure::Db {
    GraphDb::open(dir).expect("reopen store")
}

#[test]
fn known_symbol_is_redirected() {
    let dir = seed("known");
    let db = open(&dir);
    let message = decide(&db, &grep("render_map")).expect("a known symbol is redirected");
    assert_eq!(
        message,
        "mushroomdb: 'render_map' is a known symbol — call explore(\"render_map\") for its \
         definition, callers and callees instead of grepping the tree."
    );
}

#[test]
fn regex_and_unknown_patterns_pass_through() {
    let dir = seed("passthrough");
    let db = open(&dir);
    // Regex metacharacters: the caller is searching, not naming.
    for pattern in ["render_.*", "render_map|id", "^render_map$", "render map"] {
        assert_eq!(decide(&db, &grep(pattern)), None, "pattern {pattern:?}");
    }
    // A bare identifier the graph has never heard of.
    assert_eq!(decide(&db, &grep("nosuchthing")), None);
}

#[test]
fn short_names_and_malformed_payloads_pass_through() {
    let dir = seed("malformed");
    let db = open(&dir);
    // `id` is in the store, but two characters are not enough evidence that a
    // search for them meant the symbol.
    assert_eq!(decide(&db, &grep("id")), None);
    // A leading digit is not an identifier.
    assert_eq!(decide(&db, &grep("1render_map")), None);
    // Nothing to read: no payload, no `tool_input`, a non-string pattern.
    assert_eq!(decide(&db, &json!({})), None);
    assert_eq!(decide(&db, &json!({"tool_name": "Grep"})), None);
    assert_eq!(
        decide(&db, &json!({"tool_input": {"pattern": 7}})),
        None,
        "a non-string pattern is not a name"
    );
}

#[test]
fn the_hook_body_is_silent_on_a_missing_store_or_bad_json() {
    let dir = seed("hook");
    assert!(
        run_intercept(
            &dir,
            r#"{"tool_name":"Grep","tool_input":{"pattern":"render_map"}}"#
        )
        .is_some(),
        "a known symbol reaches the hook body"
    );
    assert_eq!(
        run_intercept(&dir, "not json at all"),
        None,
        "a payload that will not parse never blocks a tool call"
    );
    assert_eq!(
        run_intercept(&tmp("absent"), r#"{"tool_input":{"pattern":"render_map"}}"#),
        None,
        "no store means no opinion"
    );
}

/// A hook left behind by an uninstall runs in front of every `Grep`. It must
/// stay silent — and, above all, it must not create the store it was pointed
/// at: `RealFs::new` runs `create_dir_all`, so an unguarded open would leave
/// an empty `mushroom-memory/` in the user's repository.
#[test]
fn a_missing_store_is_never_created_by_the_hook() {
    let absent = tmp("never-created");
    assert_eq!(
        run_intercept(
            &absent,
            r#"{"tool_name":"Grep","tool_input":{"pattern":"render_map"}}"#
        ),
        None,
        "no store, no redirect"
    );
    assert!(!absent.exists(), "the hook created {}", absent.display());

    // And through the binary, which is how the hook actually runs: exit 0,
    // nothing on either stream, and still no directory.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("intercept")
        .arg(&absent)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(br#"{"tool_name":"Grep","tool_input":{"pattern":"render_map"}}"#)?;
            child.wait_with_output()
        })
        .expect("run the hook");
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    assert!(!absent.exists(), "the binary created {}", absent.display());
}
