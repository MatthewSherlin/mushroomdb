//! `mushroomdb enrich` — the optional `PostToolUse` hook body for `Grep`.
//!
//! Claude Code hands it the finished tool call as JSON on stdin. When the
//! pattern, or a name in the matches, is a symbol the graph holds, the hook
//! prints one `hookSpecificOutput` object carrying what the graph knows about
//! those symbols as `additionalContext` and exits 0. Nothing resolves →
//! nothing printed.

use cli::enrich::{self, MAX_CONTEXT_BYTES};
use core_api::{GraphDb, Value};
use std::path::{Path, PathBuf};

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mushroomdb-enrich-{name}-{seq}-{nanos}-{}",
        std::process::id()
    ))
}

/// A store with one file, three symbols in it, one caller and a top author.
fn seed(name: &str) -> PathBuf {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).expect("open store");
    db.insert_node("File", "src/render.rs", vec![])
        .expect("insert file");
    db.insert_node("File", "src/main.rs", vec![])
        .expect("insert file");
    db.insert_node(
        "Author",
        "author:ada",
        vec![("name".to_string(), Value::Str("Ada".to_string()))],
    )
    .expect("insert author");
    db.insert_edge("TOP_AUTHOR", "src/render.rs", "author:ada")
        .expect("insert top author");
    for (key, name, file, line) in [
        (
            "src/render.rs::render_map",
            "render_map",
            "src/render.rs",
            42,
        ),
        (
            "src/render.rs::render_impact",
            "render_impact",
            "src/render.rs",
            90,
        ),
        ("src/main.rs::main", "main", "src/main.rs", 1),
    ] {
        db.insert_node(
            "Symbol",
            key,
            vec![
                ("name".to_string(), Value::Str(name.to_string())),
                ("file_id".to_string(), Value::Str(file.to_string())),
                ("line_start".to_string(), Value::Int(line)),
                ("line_end".to_string(), Value::Int(line + 10)),
            ],
        )
        .expect("insert symbol");
    }
    db.insert_edge("DEFINES", "src/render.rs", "src/render.rs::render_map")
        .expect("defines");
    db.insert_edge("DEFINES", "src/render.rs", "src/render.rs::render_impact")
        .expect("defines");
    db.insert_edge("DEFINES", "src/main.rs", "src/main.rs::main")
        .expect("defines");
    db.insert_edge("CALLS", "src/main.rs::main", "src/render.rs::render_map")
        .expect("calls");
    dir
}

fn payload(pattern: &str, response: serde_json::Value) -> String {
    serde_json::json!({
        "tool_name": "Grep",
        "tool_input": {"pattern": pattern},
        "tool_response": response,
    })
    .to_string()
}

#[test]
fn the_pattern_itself_is_looked_up() {
    let dir = seed("pattern");
    let text = enrich::run(&dir, &payload("render_map", serde_json::Value::Null))
        .expect("a known symbol resolves");
    assert!(text.starts_with("about the symbols grep found:"), "{text}");
    assert!(
        text.contains(
            "src/render.rs::render_map — defined at src/render.rs:42, 1 callers, owner Ada"
        ),
        "{text}"
    );
    assert!(text.len() <= MAX_CONTEXT_BYTES, "{} bytes", text.len());
}

/// A regex pattern names nothing, but the matched lines do: the identifiers in
/// the result text are looked up too.
#[test]
fn identifiers_in_the_result_text_are_looked_up() {
    let dir = seed("result");
    let text = enrich::run(
        &dir,
        &payload(
            "render_.*",
            serde_json::json!("src/render.rs:42:fn render_map(db: &Db) {}\nsrc/render.rs:90:pub fn render_impact() {}"),
        ),
    )
    .expect("names in the matches resolve");
    assert!(text.contains("render_map"), "{text}");
    assert!(text.contains("render_impact"), "{text}");
    assert!(text.len() <= MAX_CONTEXT_BYTES, "{} bytes", text.len());
}

#[test]
fn nothing_resolvable_is_silent() {
    let dir = seed("silent");
    assert_eq!(
        enrich::run(&dir, &payload("nosuchthing", serde_json::Value::Null)),
        None
    );
    assert_eq!(enrich::run(&dir, "not json at all"), None);
    assert_eq!(enrich::run(&dir, "{}"), None);
    // Two characters are not evidence a symbol was meant, same rule the
    // redirect uses.
    assert_eq!(
        enrich::run(&dir, &payload("ma", serde_json::Value::Null)),
        None
    );
}

#[test]
fn a_missing_store_is_never_created_by_the_hook() {
    let absent = tmp("never-created");
    assert_eq!(
        enrich::run(&absent, &payload("render_map", serde_json::Value::Null)),
        None
    );
    assert!(!absent.exists(), "the hook created {}", absent.display());
}

/// At most five symbols, whatever the matches name.
#[test]
fn at_most_five_symbols_are_reported() {
    let dir = tmp("five");
    let mut db = GraphDb::open(&dir).expect("open store");
    db.insert_node("File", "src/lib.rs", vec![]).unwrap();
    let mut names = Vec::new();
    for i in 0..9 {
        let name = format!("symbol_{i:02}");
        let key = format!("src/lib.rs::{name}");
        db.insert_node(
            "Symbol",
            &key,
            vec![
                ("name".to_string(), Value::Str(name.clone())),
                ("file_id".to_string(), Value::Str("src/lib.rs".to_string())),
                ("line_start".to_string(), Value::Int(i + 1)),
                ("line_end".to_string(), Value::Int(i + 2)),
            ],
        )
        .unwrap();
        db.insert_edge("DEFINES", "src/lib.rs", &key).unwrap();
        names.push(name);
    }
    drop(db);

    let text = enrich::run(
        &dir,
        &payload("symbol_.*", serde_json::json!(names.join(" "))),
    )
    .expect("nine names, five reported");
    let reported = names.iter().filter(|n| text.contains(n.as_str())).count();
    assert_eq!(reported, 5, "{text}");
    assert!(text.len() <= MAX_CONTEXT_BYTES, "{} bytes", text.len());
}

/// The exact stdout contract, pinned.
#[test]
fn the_binary_prints_the_documented_posttooluse_object() {
    let dir = seed("bin");
    let out = run_hook(&dir, &payload("render_map", serde_json::Value::Null));
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let printed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    let specific = &printed["hookSpecificOutput"];
    assert_eq!(specific["hookEventName"], "PostToolUse");
    assert!(specific["additionalContext"]
        .as_str()
        .is_some_and(|c| c.starts_with("about the symbols grep found:")));
    assert_eq!(printed.as_object().unwrap().len(), 1, "{printed}");
    assert_eq!(specific.as_object().unwrap().len(), 2, "{specific}");

    let out = run_hook(&dir, &payload("nosuchthing", serde_json::Value::Null));
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

fn run_hook(dir: &Path, payload: &str) -> std::process::Output {
    use std::io::Write as _;
    std::process::Command::new(env!("CARGO_BIN_EXE_mushroomdb"))
        .arg("enrich")
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
