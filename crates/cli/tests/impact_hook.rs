//! `mushroomdb impact-hook` — the optional `PreToolUse` hook body for edits.
//!
//! Claude Code hands it the tool call as JSON on stdin before an `Edit`,
//! `Write` or `MultiEdit` lands. When the graph knows the file, the hook
//! prints one `hookSpecificOutput` object carrying the blast radius as
//! `additionalContext` and exits 0 — the edit still happens, the model just
//! knows what it reaches. Everything else is silence and exit 0.

use cli::impact_hook::{self, MAX_CONTEXT_BYTES};
use core_api::{GraphDb, Value};
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
        "mushroomdb-impact-hook-{name}-{}-{nanos}-{seq}",
        std::process::id()
    ))
}

/// A small store shaped like a synced repository: a `GitSync` marker naming a
/// repo root, three files, an import edge, a co-change edge and a test file.
fn seed(name: &str, repo: &Path) -> PathBuf {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).expect("open store");
    db.insert_node(
        "GitSync",
        "__mushroomdb_git_sync__",
        vec![(
            "repo".to_string(),
            Value::Str(repo.to_string_lossy().into_owned()),
        )],
    )
    .expect("insert marker");
    // Three commits `install.rs` and `doctor.rs` share, which is what makes
    // them co-change partners (`MIN_SHARED_COMMITS`).
    let shas: Vec<String> = (0..3).map(|i| format!("sha{i}")).collect();
    let commits = Value::List(shas.iter().map(|s| Value::Str(s.clone())).collect());
    for path in [
        "src/install.rs",
        "src/main.rs",
        "src/doctor.rs",
        "tests/install.rs",
    ] {
        let props = match path {
            "src/install.rs" | "src/doctor.rs" => {
                vec![("commits".to_string(), commits.clone())]
            }
            _ => vec![],
        };
        db.insert_node("File", path, props).expect("insert file");
    }
    for sha in &shas {
        db.insert_node("Commit", sha, vec![])
            .expect("insert commit");
        for path in ["src/install.rs", "src/doctor.rs"] {
            db.insert_edge("TOUCHED", sha, path)
                .expect("insert touched");
        }
    }
    // `main.rs` and `tests/install.rs` import the edited file.
    for importer in ["src/main.rs", "tests/install.rs"] {
        db.insert_edge("IMPORTS", importer, "src/install.rs")
            .expect("insert import");
    }
    dir
}

fn payload(file_path: &str) -> String {
    serde_json::json!({
        "tool_name": "Edit",
        "tool_input": {"file_path": file_path, "old_string": "a", "new_string": "b"}
    })
    .to_string()
}

#[test]
fn a_known_file_gets_its_blast_radius() {
    let repo = tmp("repo-known");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = seed("known", &repo);

    let text =
        impact_hook::run(&dir, &payload("src/install.rs")).expect("a known file is reported");
    assert!(
        text.starts_with("impact of editing src/install.rs:"),
        "{text}"
    );
    // `(1)` counts the non-test importers this line names, not the file's
    // fan-in: `tests/install.rs` is the other importer and is reported as a
    // test. No `+more`, because the graph's importer list came back short of
    // its cap, so the count is complete.
    assert!(text.contains("callers src/main.rs (1)"), "{text}");
    assert!(!text.contains("+more"), "{text}");
    assert!(text.contains("changes with src/doctor.rs"), "{text}");
    assert!(text.contains("tests tests/install.rs"), "{text}");
    assert!(text.len() <= MAX_CONTEXT_BYTES, "{} bytes", text.len());
}

/// When `impact` returns a full importer list the true fan-in is unknown — the
/// engine stopped at its cap — so the line says so rather than reporting the
/// cap as if it were the answer.
#[test]
fn a_truncated_importer_list_is_marked_rather_than_counted() {
    let repo = tmp("repo-cap");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = tmp("cap");
    let mut db = GraphDb::open(&dir).expect("open store");
    db.insert_node(
        "GitSync",
        "__mushroomdb_git_sync__",
        vec![(
            "repo".to_string(),
            Value::Str(repo.to_string_lossy().into_owned()),
        )],
    )
    .expect("insert marker");
    db.insert_node("File", "src/install.rs", vec![]).unwrap();
    // Nine importers: more than `ImpactOptions::default().max_importers` (6),
    // so the report comes back capped.
    for i in 0..9 {
        let path = format!("src/importer{i}.rs");
        db.insert_node("File", &path, vec![]).unwrap();
        db.insert_edge("IMPORTS", &path, "src/install.rs").unwrap();
    }
    drop(db);

    let text = impact_hook::run(&dir, &payload("src/install.rs")).expect("importers are reported");
    assert!(text.contains("(6+more)"), "{text}");
    assert!(text.len() <= MAX_CONTEXT_BYTES, "{} bytes", text.len());
}

/// An absolute path under the recorded repo root is the shape Claude Code
/// actually sends; a path relative to that root is accepted too.
#[test]
fn absolute_paths_under_the_repo_root_resolve() {
    let repo = tmp("repo-abs");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = seed("abs", &repo);

    let abs = repo.join("src/install.rs");
    let text = impact_hook::run(&dir, &payload(&abs.to_string_lossy())).expect("absolute resolves");
    assert!(
        text.starts_with("impact of editing src/install.rs:"),
        "{text}"
    );
    // A `./`-prefixed relative path is the same file.
    assert!(impact_hook::run(&dir, &payload("./src/install.rs")).is_some());
}

#[test]
fn an_unknown_file_a_missing_store_and_bad_json_are_silent() {
    let repo = tmp("repo-silent");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = seed("silent", &repo);

    assert_eq!(impact_hook::run(&dir, &payload("src/nowhere.rs")), None);
    assert_eq!(impact_hook::run(&dir, "not json at all"), None);
    assert_eq!(impact_hook::run(&dir, "{}"), None);
    assert_eq!(
        impact_hook::run(&dir, r#"{"tool_input":{"file_path":7}}"#),
        None
    );
    // A path outside the recorded repository is not this repository's file.
    assert_eq!(impact_hook::run(&dir, &payload("/etc/hosts")), None);
}

/// A hook left behind by an uninstall runs before every edit. It must stay
/// silent and must never create the store it was pointed at.
#[test]
fn a_missing_store_is_never_created_by_the_hook() {
    let absent = tmp("never-created");
    assert_eq!(impact_hook::run(&absent, &payload("src/install.rs")), None);
    assert!(!absent.exists(), "the hook created {}", absent.display());
}

/// The exact stdout contract, pinned: one line of JSON with the documented
/// `hookSpecificOutput` shape, and exit 0 whatever happens.
#[test]
fn the_binary_prints_the_documented_pretooluse_object() {
    let repo = tmp("repo-bin");
    std::fs::create_dir_all(&repo).unwrap();
    let dir = seed("bin", &repo);

    let out = run_hook(&dir, &payload("src/install.rs"));
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let printed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    let specific = &printed["hookSpecificOutput"];
    assert_eq!(specific["hookEventName"], "PreToolUse");
    let context = specific["additionalContext"]
        .as_str()
        .expect("additionalContext is a string");
    assert!(
        context.starts_with("impact of editing src/install.rs:"),
        "{context}"
    );
    assert_eq!(
        printed.as_object().unwrap().len(),
        1,
        "nothing but hookSpecificOutput: {printed}"
    );
    assert_eq!(
        specific.as_object().unwrap().len(),
        2,
        "nothing but the event name and the context: {specific}"
    );

    // Nothing to say is nothing printed.
    let out = run_hook(&dir, &payload("src/nowhere.rs"));
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

fn run_hook(dir: &Path, payload: &str) -> std::process::Output {
    use std::io::Write as _;
    std::process::Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("impact-hook")
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(payload.as_bytes())?;
            child.wait_with_output()
        })
        .expect("run the hook")
}
