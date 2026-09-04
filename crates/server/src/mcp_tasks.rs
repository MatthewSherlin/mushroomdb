//! The eight MCP tools that answer a question about a graphed repository.
//!
//! `map`, `context`, `impact`, `owners`, `why`, `recall`, `remember` and
//! `sync` sit in front of the sixteen graph tools in `mcp::tools_list`, because
//! they are what an assistant working in a checkout actually reaches for: what
//! is this repository, what is this symbol, what does my diff touch, who wrote
//! this, why are these two linked, what do I already know, remember this, and
//! bring the store up to date.
//!
//! # Shape of a reply
//!
//! Every tool here answers with the rendered digest as its text content, and
//! the serialised report — carrying that same text under `text` — as
//! `structuredContent`. An assistant reads the digest; a program that wants the
//! numbers reads the report; neither has to parse the other.
//!
//! # What each one reads and writes
//!
//! All but two are pure reads of the graph. `remember` writes one `Note`, and
//! `sync` writes nothing itself: it runs this binary again as
//! `<exe> sync <db> --json` and hands back what that reports. The server crate
//! cannot depend on the CLI crate that owns the incremental ingest, and
//! re-implementing it here would give two answers to one question.
//!
//! # Reading the working tree
//!
//! Two tools look outside the graph. `context` quotes source from the
//! repository the `GitSync` marker names, which core-api does for us. `impact`
//! defaults its file list to the current diff, taken from `$CLAUDE_PROJECT_DIR`
//! when the host sets it to a checkout and from that same marker otherwise;
//! with neither, it says to pass files explicitly rather than guessing.
//!
//! # Untrusted content
//!
//! Everything these tools render came out of the graph, and a graph built by
//! `ingest-git` holds whatever contributors wrote: author names, paths, commit
//! subjects, doc comments, and — through `context` — lines of the working tree.
//! [`ok`] therefore stamps every reply with
//! [`repograph::UNTRUSTED_FRAMING`], the same marker
//! `recall_digest` puts on its own digest, so an assistant is told to read the
//! lines under it as data before it reads any of them. The renderers already
//! sanitize each line; the framing is what says whose words they are.

use crate::mcp::CallOutcome;
use core_api::repograph::{
    self, ImpactOptions, MapOptions, RememberInput, DEFAULT_EXCLUDES, MAX_OUTPUT_BYTES, NOTE_KINDS,
    UNTRUSTED_FRAMING,
};
use core_api::{GraphError, SharedDb};
use serde_json::{json, Value as Js};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `GitSync` marker `ingest-git` writes, and the prop naming the checkout.
const SYNC_KEY: &str = "__mushroomdb_git_sync__";
const SYNC_REPO_PROP: &str = "repo";

/// The host's project directory: the checkout an assistant is working in.
const PROJECT_DIR_VAR: &str = "CLAUDE_PROJECT_DIR";

/// Route a task tool. `None` when `name` is not one of the eight.
pub(crate) fn dispatch(
    db: &SharedDb,
    db_dir: Option<&Path>,
    name: &str,
    args: &Js,
) -> Option<CallOutcome> {
    Some(match name {
        "map" => tool_map(db),
        "context" => tool_context(db, args),
        // The one environment read on this path, done here so every function
        // below takes the value and can be tested without touching the
        // process environment.
        "impact" => tool_impact(db, args, std::env::var_os(PROJECT_DIR_VAR).as_deref()),
        "owners" => tool_owners(db, args),
        "why" => tool_why(db, args),
        "recall" => tool_recall(db, db_dir, args),
        "remember" => tool_remember(db, args),
        "sync" => tool_sync(db_dir),
        _ => return None,
    })
}

/// A successful task reply: the rendered digest under the untrusted-data
/// framing line, plus the report with that same framed text under `text`.
///
/// `recall_digest` emits the framing itself, so a digest that already carries
/// it is left alone rather than marked twice.
fn ok(text: String, structured: Js) -> CallOutcome {
    let text = if text.starts_with(UNTRUSTED_FRAMING) {
        text
    } else {
        format!("{UNTRUSTED_FRAMING}{text}")
    };
    let mut structured = match structured {
        Js::Object(map) => Js::Object(map),
        other => json!({ "report": other }),
    };
    if let Some(obj) = structured.as_object_mut() {
        obj.insert("text".to_string(), Js::String(text.clone()));
    }
    CallOutcome::TaskOk { text, structured }
}

/// Serialise a report, or report the failure as a tool error rather than
/// dropping the answer.
fn to_js<T: serde::Serialize>(report: &T) -> Result<Js, String> {
    serde_json::to_value(report).map_err(|e| format!("serialise report: {e}"))
}

/// A required string argument.
fn str_arg<'a>(args: &'a Js, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Js::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

/// An optional array-of-strings argument. `Err` when present but wrong-typed.
fn str_list_arg(args: &Js, name: &str) -> Result<Vec<String>, String> {
    let Some(v) = args.get(name) else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| format!("{name} must be an array of strings"))?;
    arr.iter()
        .map(|x| {
            x.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{name} must be an array of strings"))
        })
        .collect()
}

// ── map ──────────────────────────────────────────────────────────────────────

fn tool_map(db: &SharedDb) -> CallOutcome {
    let map = {
        let g = db.read();
        repograph::repo_map(&*g, &MapOptions::default())
    };
    match to_js(&map) {
        Ok(structured) => ok(repograph::render_map(&map), structured),
        Err(e) => CallOutcome::ToolErr(e),
    }
}

// ── context ──────────────────────────────────────────────────────────────────

fn tool_context(db: &SharedDb, args: &Js) -> CallOutcome {
    let target = match str_arg(args, "target") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    // `None` for the repository: core-api falls back to the `GitSync` marker,
    // which is the checkout the store was built from.
    let report = {
        let g = db.read();
        repograph::context(&*g, None, target)
    };
    match to_js(&report) {
        Ok(structured) => ok(repograph::render_context(&report), structured),
        Err(e) => CallOutcome::ToolErr(e),
    }
}

// ── impact ───────────────────────────────────────────────────────────────────

/// `project_dir` is the value of `$CLAUDE_PROJECT_DIR`, passed in rather than
/// read here so a test can exercise both branches of [`project_repo`] without
/// mutating the process environment.
fn tool_impact(db: &SharedDb, args: &Js, project_dir: Option<&OsStr>) -> CallOutcome {
    let mut files = match str_list_arg(args, "files") {
        Ok(f) => f,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    if files.is_empty() {
        let repo = match project_repo(db, project_dir) {
            Some(r) => r,
            None => {
                return CallOutcome::ToolErr(
                    "no repository to read a diff from: pass files explicitly".into(),
                )
            }
        };
        match changed_paths(&repo) {
            Ok(paths) => files = paths,
            Err(e) => {
                return CallOutcome::ToolErr(format!(
                    "could not read the diff in {}: {e}; pass files explicitly",
                    repo.display()
                ))
            }
        }
    }
    // The caller's whole change is also what decides the `modified` flag: a
    // partner that is itself being edited is a different fact from one that is
    // not, and only this set can tell them apart.
    let modified: BTreeSet<String> = files.iter().cloned().collect();
    let report = {
        let g = db.read();
        repograph::impact(&*g, &files, &modified, &ImpactOptions::default())
    };
    match to_js(&report) {
        Ok(structured) => ok(repograph::render_impact(&report), structured),
        Err(e) => CallOutcome::ToolErr(e),
    }
}

/// The checkout root a default `impact` reads its diff from: the host's
/// project directory when it named one inside a repository, else the
/// repository the store was built from.
///
/// `$CLAUDE_PROJECT_DIR` wins because an assistant asking "what does my change
/// touch" means the tree it is editing, which is where the host put it. It has
/// to be inside a checkout to win, though: a host that points it at a plain
/// directory has said nothing about the repository the store knows, so the
/// marker still answers rather than the call failing.
///
/// Both branches resolve to the repository **root**, not to the directory that
/// named it, so the two listings in [`changed_paths`] agree about what their
/// paths are relative to — and so those paths match `File` keys, which are
/// root-relative.
fn project_repo(db: &SharedDb, project_dir: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(root) = project_dir.map(Path::new).and_then(repo_root) {
        return Some(root);
    }
    let repo = {
        let g = db.read();
        g.node_ref(SYNC_KEY)
            .and_then(|n| n.prop(SYNC_REPO_PROP))
            .and_then(|v| match v {
                core_api::Value::Str(s) => Some(s),
                _ => None,
            })
    }?;
    repo_root(Path::new(&repo))
}

/// The root of the checkout `dir` is in, or `None` when it is not in one.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Paths under the checkout rooted at `root` that differ from `HEAD` or are not
/// tracked at all: root-relative, sorted, deduplicated, and filtered by the
/// same [`DEFAULT_EXCLUDES`] the ingest applied.
///
/// The exclusion matters because a path the ingest skipped is a path no `File`
/// node exists for, and reporting it back as `unknown:` reads like a hole in
/// the graph rather than a build artefact the store never wanted.
///
/// `-z` rather than the default listing: git escapes and quotes a path holding
/// a tab, a newline or a non-ASCII byte, and a quoted path matches no key.
/// `root` rather than the directory the caller named: `ls-files` lists relative
/// to the working directory while `diff` lists relative to the root, so running
/// both anywhere but the root would mix two conventions in one list.
fn changed_paths(root: &Path) -> Result<Vec<String>, String> {
    const LISTS: [&[&str]; 2] = [
        &["diff", "--name-only", "-z", "HEAD"],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ];
    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|p| (*p).to_string()).collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut ran = false;
    for args in LISTS {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        // `diff HEAD` fails in a repository with no commits yet. Nothing is
        // dirty relative to a head that does not exist, so that is not an error
        // — but if *neither* listing runs, this is not a repository at all.
        if !output.status.success() {
            continue;
        }
        ran = true;
        for path in String::from_utf8_lossy(&output.stdout).split('\0') {
            if !path.is_empty() && !repograph::path_excluded(path, &excludes) {
                out.insert(path.to_string());
            }
        }
    }
    if !ran {
        return Err("git listed nothing there".into());
    }
    Ok(out.into_iter().collect())
}

// ── owners ───────────────────────────────────────────────────────────────────

fn tool_owners(db: &SharedDb, args: &Js) -> CallOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let report = {
        let g = db.read();
        repograph::owners(&*g, path, None)
    };
    let Some(report) = report else {
        return CallOutcome::ToolErr(format!("no file in the store at {path}"));
    };
    match to_js(&report) {
        Ok(structured) => ok(repograph::render_owners(&report), structured),
        Err(e) => CallOutcome::ToolErr(e),
    }
}

// ── why ──────────────────────────────────────────────────────────────────────

fn tool_why(db: &SharedDb, args: &Js) -> CallOutcome {
    let a = match str_arg(args, "a") {
        Ok(v) => v.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let b = match str_arg(args, "b") {
        Ok(v) => v.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    // Keys the graph does not hold are an answer, not a failure: the report
    // names them and the digest says `unknown:`, which tells the caller which
    // of the two to fix.
    let report = {
        let g = db.read();
        repograph::why(&*g, &a, &b)
    };
    match to_js(&report) {
        Ok(structured) => ok(repograph::render_why(&report), structured),
        Err(e) => CallOutcome::ToolErr(e),
    }
}

// ── recall ───────────────────────────────────────────────────────────────────

fn tool_recall(db: &SharedDb, db_dir: Option<&Path>, args: &Js) -> CallOutcome {
    let topic = match str_arg(args, "topic") {
        Ok(t) => t.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let label = db_dir.map_or_else(|| "store".to_string(), |d| d.display().to_string());
    // The same rewrite the `recall` hook applies to a prompt: terms inside one
    // full-text group are ANDed, so raw prose matches nothing.
    let digest = match repograph::or_query(&topic) {
        Some(query) => {
            let g = db.read();
            repograph::recall_digest(&*g, &query, &label, MAX_OUTPUT_BYTES)
        }
        None => String::new(),
    };
    let text = if digest.is_empty() {
        format!(
            "mushroomdb recall — nothing indexed matches {}\n",
            repograph::sanitize(&topic)
        )
    } else {
        digest.clone()
    };
    ok(text, json!({ "topic": topic, "digest": digest }))
}

// ── remember ─────────────────────────────────────────────────────────────────

fn tool_remember(db: &SharedDb, args: &Js) -> CallOutcome {
    let text = match str_arg(args, "text") {
        Ok(t) => t.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let mut about = match str_list_arg(args, "about") {
        Ok(a) => a,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    about.sort();
    about.dedup();
    let kind = match args.get("kind") {
        None | Some(Js::Null) => "note".to_string(),
        Some(Js::String(k)) => k.clone(),
        Some(_) => return CallOutcome::ToolErr("kind must be a string".into()),
    };
    if !NOTE_KINDS.contains(&kind.as_str()) {
        return CallOutcome::ToolErr(format!(
            "kind must be one of {}, got {kind:?}",
            NOTE_KINDS.join(", ")
        ));
    }

    // The engine names the first missing key, which makes a caller with three
    // bad ones retry three times. Check them all here and name them all at
    // once, before anything is written.
    let missing: Vec<String> = {
        let g = db.read();
        about
            .iter()
            .filter(|k| !g.has_node(k))
            .map(|k| repograph::sanitize(k))
            .collect()
    };
    if !missing.is_empty() {
        return CallOutcome::ToolErr(format!(
            "unknown about {}: {}",
            if missing.len() == 1 { "key" } else { "keys" },
            missing.join(", ")
        ));
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let input = RememberInput {
        text: &text,
        about: &about,
        kind: &kind,
        ts,
    };
    let key = {
        let mut g = db.write();
        repograph::remember(&mut *g, &input)
    };
    match key {
        Ok(key) => {
            let mut rendered = format!("remembered {}\n", repograph::sanitize(&key));
            if !about.is_empty() {
                rendered.push_str(&format!(
                    "about  {}\n",
                    about
                        .iter()
                        .map(|k| repograph::sanitize(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            ok(
                rendered,
                json!({ "key": key, "kind": kind, "about": about }),
            )
        }
        Err(e) => CallOutcome::ToolErr(match e {
            GraphError::QueryError { detail } | GraphError::IngestError { detail } => detail,
            other => other.to_string(),
        }),
    }
}

// ── sync ─────────────────────────────────────────────────────────────────────

/// Run the incremental ingest and report what it did.
///
/// The child is waited on to completion. A full sync of a large repository is
/// real work, and an assistant that asked for one is waiting on the answer;
/// cutting it off part-way would leave the store half-updated with nothing said
/// about it. The MCP loop is single-threaded, so nothing else is served while
/// it runs — which is correct, since every other tool would be answering from
/// the store the child is rewriting.
fn tool_sync(db_dir: Option<&Path>) -> CallOutcome {
    let Some(db_dir) = db_dir else {
        return CallOutcome::ToolErr(
            "store path unknown: sync needs the directory this server was started on".into(),
        );
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return CallOutcome::ToolErr(format!("sync cannot find this binary: {e}")),
    };
    // The incremental ingest lives in the CLI crate, which the server cannot
    // depend on, so `sync` re-runs this same binary. Under the npx launcher
    // `current_exe()` is already the native binary rather than the shim.
    let output = match Command::new(&exe)
        .arg("sync")
        .arg(db_dir)
        .arg("--json")
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            return CallOutcome::ToolErr(format!("sync could not run {}: {e}", exe.display()))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("exit {}", output.status)
        } else {
            repograph::sanitize(detail)
        };
        return CallOutcome::ToolErr(format!("sync failed: {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Ok(Js::Object(report)) = serde_json::from_str::<Js>(stdout.trim()) else {
        return CallOutcome::ToolErr(format!(
            "sync produced no report: {}",
            repograph::sanitize(stdout.trim())
        ));
    };
    // The CLI already rendered the digest into the object, so both halves of
    // the reply come from the one run.
    let text = report
        .get("text")
        .and_then(Js::as_str)
        .unwrap_or_default()
        .to_string();
    ok(text, Js::Object(report))
}

// ── tools/list ───────────────────────────────────────────────────────────────

/// The eight task tools, in the order `tools/list` puts them: the question an
/// assistant asks first comes first.
pub(crate) fn task_tools() -> Vec<Js> {
    vec![
        json!({
            "name": "map",
            "description": "Summarise the graphed repository in one screen: size, last sync, clusters, key files, owners, hot files, stale concepts, and questions worth asking next. Start here when you do not know the codebase.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "context",
            "description": "Everything known about one file or symbol: signature, doc, source from the working tree, owner, callers and callees, importers and imports, co-change partners, recent commits, and any notes or concepts about it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A file path, a symbol key (path#name), or a bare symbol name. An ambiguous bare name returns the candidates instead."
                    }
                },
                "required": ["target"]
            }
        }),
        json!({
            "name": "impact",
            "description": "What else the files in a change reach: co-change partners with scores, importers, symbols used elsewhere, and each file's owner. Defaults to the current git diff plus untracked files when no list is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Repository-relative paths. Omit to use the working tree's diff against HEAD plus its untracked files."
                    }
                }
            }
        }),
        json!({
            "name": "owners",
            "description": "Who has written a file: top author and share, authors who know it, the last commit to touch it, and the split by quarter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Repository-relative file path."
                    }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "why",
            "description": "What links two files, symbols, or people, with the evidence for each link: shared commits, the importing line, the calling line, the file two authors both know. Falls back to the shortest path between them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "a": { "type": "string", "minLength": 1, "description": "First node key." },
                    "b": { "type": "string", "minLength": 1, "description": "Second node key." }
                },
                "required": ["a", "b"]
            }
        }),
        json!({
            "name": "recall",
            "description": "What the graph already knows about a topic: the closest notes, concepts, files, symbols and people, each with its strongest link.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Free-form text. Searched as an OR of its words."
                    }
                },
                "required": ["topic"]
            }
        }),
        json!({
            "name": "remember",
            "description": "Write a note into the graph and return its key. Keys listed in 'about' are linked to the note, and every one of them must already exist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The note itself, 1 to 4000 characters."
                    },
                    "about": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Existing node keys the note is about: files, symbols, authors, concepts, other notes."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["note", "decision", "todo"],
                        "description": "What kind of note this is (default: note)."
                    }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "sync",
            "description": "Bring the store up to date with the repository it was built from: the commits since the last sync, then the files that differ from HEAD. Returns what changed.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests: the two things these tools decide before they touch the graph — where
// a default `impact` reads its diff from, and how that diff is filtered.
//
// They live here rather than in `tests/mcp.rs` because `$CLAUDE_PROJECT_DIR`
// reaches `tool_impact` as an argument, not as a process-global read: setting
// it for real would race every other test in the binary that calls
// `std::env::temp_dir()`.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use core_api::Value;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(name: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("mcp-tasks-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A checkout holding one committed file, since edited, plus one untracked
    /// file under an excluded directory.
    fn dirty_repo(name: &str) -> PathBuf {
        let repo = tmp(name);
        std::fs::create_dir_all(repo.join("src")).expect("src");
        std::fs::create_dir_all(repo.join("target")).expect("target");
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@example.test"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("src/core.rs"), "fn init() {}\n").expect("write");
        git(&repo, &["add", "src/core.rs"]);
        git(&repo, &["commit", "-qm", "first"]);
        std::fs::write(repo.join("src/core.rs"), "fn init() { /* edited */ }\n").expect("edit");
        // Untracked and excluded at ingest time, so it must not reach the list.
        std::fs::write(repo.join("target/debug.log"), "noise\n").expect("artefact");
        repo
    }

    /// A store holding one `File` node and a `GitSync` marker pointing at `repo`.
    fn store_for(name: &str, repo: Option<&Path>) -> (SharedDb, PathBuf) {
        let dir = tmp(name);
        let db = SharedDb::open(&dir).expect("open");
        {
            let mut w = db.write();
            w.insert_node(
                "File",
                "src/core.rs",
                vec![
                    ("id".into(), Value::Str("src/core.rs".into())),
                    ("path".into(), Value::Str("src/core.rs".into())),
                    ("lines".into(), Value::Int(1)),
                ],
            )
            .expect("file");
            let marker = repo.map_or_else(
                || "/nonexistent/mushroomdb-test-repo".to_string(),
                |r| r.display().to_string(),
            );
            w.insert_node(
                "GitSync",
                SYNC_KEY,
                vec![
                    ("id".into(), Value::Str(SYNC_KEY.into())),
                    (SYNC_REPO_PROP.into(), Value::Str(marker)),
                ],
            )
            .expect("marker");
        }
        (db, dir)
    }

    fn impact_files(outcome: &CallOutcome) -> Vec<String> {
        match outcome {
            CallOutcome::TaskOk { structured, .. } => structured["files"]
                .as_array()
                .expect("files")
                .iter()
                .map(|f| f["path"].as_str().expect("path").to_string())
                .collect(),
            other => panic!("expected a task result, got {}", describe(other)),
        }
    }

    fn describe(outcome: &CallOutcome) -> String {
        match outcome {
            CallOutcome::ToolErr(m) => format!("tool error: {m}"),
            CallOutcome::TaskOk { text, .. } => format!("ok: {text}"),
            CallOutcome::ToolOk(v) => format!("json: {v}"),
            CallOutcome::Protocol { message, .. } => format!("protocol: {message}"),
        }
    }

    /// Binding: with no `files`, the diff comes from the checkout the marker
    /// names, and excluded artefacts are left out of it.
    #[test]
    fn default_files_come_from_the_marker_repo_and_skip_excluded_paths() {
        let repo = dirty_repo("marker-repo");
        let (db, dir) = store_for("marker-store", Some(&repo));

        let outcome = tool_impact(&db, &json!({}), None);
        assert_eq!(
            impact_files(&outcome),
            vec!["src/core.rs".to_string()],
            "the uncommitted edit, and not the build artefact"
        );
        match &outcome {
            CallOutcome::TaskOk { structured, .. } => assert_eq!(
                structured["unknown"],
                json!([]),
                "an excluded path must not come back as unknown"
            ),
            other => panic!("{}", describe(other)),
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Binding: `$CLAUDE_PROJECT_DIR` wins over the marker when it names a
    /// checkout.
    #[test]
    fn the_project_directory_wins_over_the_marker() {
        let project = dirty_repo("project-repo");
        // The marker points somewhere that does not exist, so a result at all
        // proves the project directory was the one read.
        let (db, dir) = store_for("project-store", None);

        let outcome = tool_impact(&db, &json!({}), Some(project.as_os_str()));
        assert_eq!(impact_files(&outcome), vec!["src/core.rs".to_string()]);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&project);
    }

    /// Binding: a subdirectory of a checkout resolves to the checkout root, so
    /// both git listings agree about what their paths are relative to.
    #[test]
    fn a_project_subdirectory_resolves_to_the_repository_root() {
        let repo = dirty_repo("subdir-repo");
        let (db, dir) = store_for("subdir-store", None);

        let outcome = tool_impact(&db, &json!({}), Some(repo.join("src").as_os_str()));
        assert_eq!(
            impact_files(&outcome),
            vec!["src/core.rs".to_string()],
            "paths stay root-relative, matching File keys"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Binding: a project directory that is not inside a checkout says nothing
    /// about the store's repository, so the marker still answers.
    #[test]
    fn a_project_directory_outside_a_checkout_falls_back_to_the_marker() {
        let repo = dirty_repo("fallback-repo");
        let plain = tmp("fallback-plain");
        std::fs::create_dir_all(&plain).expect("plain dir");
        let (db, dir) = store_for("fallback-store", Some(&repo));

        let outcome = tool_impact(&db, &json!({}), Some(plain.as_os_str()));
        assert_eq!(impact_files(&outcome), vec!["src/core.rs".to_string()]);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Binding: with neither a project checkout nor a marker checkout, the tool
    /// says what the caller must do instead.
    #[test]
    fn no_checkout_anywhere_says_pass_files_explicitly() {
        let (db, dir) = store_for("no-repo-store", None);

        let outcome = tool_impact(
            &db,
            &json!({}),
            Some(OsStr::new("/nonexistent/mushroomdb-test-project")),
        );
        match &outcome {
            CallOutcome::ToolErr(m) => assert!(m.contains("pass files explicitly"), "{m}"),
            other => panic!("{}", describe(other)),
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Binding: an explicit `files` list never looks at a repository at all.
    #[test]
    fn explicit_files_ignore_the_project_directory() {
        let (db, dir) = store_for("explicit-store", None);

        let outcome = tool_impact(
            &db,
            &json!({"files": ["src/core.rs"]}),
            Some(OsStr::new("/nonexistent/mushroomdb-test-project")),
        );
        assert_eq!(impact_files(&outcome), vec!["src/core.rs".to_string()]);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
