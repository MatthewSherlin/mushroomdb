//! The ten MCP tools that answer a question in prose rather than in JSON.
//!
//! `explore`, `map`, `context`, `impact`, `owners`, `why`, `recall`,
//! `remember` and `sync` sit in front of the fifteen graph tools in
//! `mcp::tools_list`, because they are what an assistant working in a checkout
//! actually reaches for: find me this thing, what is this repository, what is
//! this symbol, what does my diff touch, who wrote this, why are these two
//! linked, what do I already know, remember this, and bring the store up to
//! date. `explain_association` is the tenth, and the one that answers on a
//! store with no repository in it: why these two entities are associated, with
//! the rule that derived each edge.
//!
//! `explore` is the composition of `context`, `impact` and `owners` behind one
//! name, and on a store a repository was ingested into it is the *only* task
//! tool `tools/list` advertises — see `mcp::Surface`. The rest stay callable
//! and are one `--all-tools` away.
//!
//! # Shape of a reply
//!
//! Every tool here answers with the rendered digest as its **text content and
//! nothing else**. It used to ship the serialised report alongside it as
//! `structuredContent`, with the same digest repeated under a `text` key: on
//! seven representative calls that was 11.6 KB of digest against 11.9 KB of
//! exact duplicate and 15.5 KB of restatement, 3.42× the text an assistant
//! reads, and it slipped past the renderers' line budgets — a default `impact`
//! capped its text at 25 lines while shipping 13 KB of uncapped report beside
//! it. No task tool declares an `outputSchema`, so nothing bound that payload.
//!
//! A program that wants the numbers asks for them: every tool takes an
//! optional `json` boolean, and with it set the reply is the serialised report
//! as the text content, with no rendered digest.
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
    self, ContextOptions, ImpactOptions, MapOptions, RememberInput, DEFAULT_EXCLUDES,
    MAX_OUTPUT_BYTES, NOTE_KINDS, UNTRUSTED_FRAMING,
};
use core_api::{Explanation, GraphError, PredicateSummary, SharedDb};
use serde_json::{json, Value as Js};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `GitSync` marker `ingest-git` writes, and the prop naming the checkout.
///
/// Its presence is also what tells a code-graph store from a memory one, which
/// is how [`mcp::Surface`](crate::mcp) picks the tools to advertise.
pub(crate) const SYNC_KEY: &str = "__mushroomdb_git_sync__";
const SYNC_REPO_PROP: &str = "repo";

/// The host's project directory: the checkout an assistant is working in.
const PROJECT_DIR_VAR: &str = "CLAUDE_PROJECT_DIR";

/// The ten names this module answers to. Listed once, so the `json` argument
/// below is read for exactly the tools that declare it.
///
/// `explore` comes first because it is the whole default surface of a
/// code-graph store: the one tool a session finds, composed from the three
/// beneath it. `explain_association` sits beside `why` because they are the
/// same question asked of the two doors: what links these two, with the
/// evidence — `why` from a code graph, `explain_association` from the rules
/// that derived the edge.
pub(crate) const TASK_TOOLS: [&str; 10] = [
    "explore",
    "map",
    "context",
    "impact",
    "owners",
    "why",
    "explain_association",
    "recall",
    "remember",
    "sync",
];

/// Route a task tool. `None` when `name` is not one of the ten.
pub(crate) fn dispatch(
    db: &SharedDb,
    db_dir: Option<&Path>,
    name: &str,
    args: &Js,
) -> Option<CallOutcome> {
    if !TASK_TOOLS.contains(&name) {
        return None;
    }
    // Every task tool takes the same optional `json`, so it is read and
    // type-checked once here rather than ten times — and before any work, so
    // a caller that mistyped it is told so rather than served a digest it did
    // not ask for.
    let json_out = match bool_arg(args, "json") {
        Ok(b) => b,
        Err(e) => return Some(CallOutcome::ToolErr(e)),
    };
    Some(match name {
        "explore" => tool_explore(db, args, json_out),
        "map" => tool_map(db, json_out),
        "context" => tool_context(db, args, json_out),
        // The one environment read on this path, done here so every function
        // below takes the value and can be tested without touching the
        // process environment.
        "impact" => tool_impact(
            db,
            args,
            std::env::var_os(PROJECT_DIR_VAR).as_deref(),
            json_out,
        ),
        "owners" => tool_owners(db, args, json_out),
        "why" => tool_why(db, args, json_out),
        "explain_association" => tool_explain_association(db, args, json_out),
        "recall" => tool_recall(db, db_dir, args, json_out),
        "remember" => tool_remember(db, args, json_out),
        "sync" => tool_sync(db_dir, json_out),
        _ => unreachable!("TASK_TOOLS and this match list the same ten names"),
    })
}

/// A successful task reply.
///
/// With `json_out` clear — the default — it is the rendered digest under the
/// untrusted-data framing line, and nothing else: no `structuredContent`, no
/// second copy of the same text. `recall_digest` emits the framing itself, so
/// a digest that already carries it is left alone rather than marked twice.
///
/// With `json_out` set it is the serialised report as the text content, for a
/// program that wants the numbers. The report is never rendered in that case,
/// so nothing is computed twice.
///
/// A JSON reply carries **no framing line**: prefixing one would stop the
/// payload being parseable, and the caller that asked for JSON asked for a
/// document to parse rather than prose to read. It is still graph content, so
/// every string in it goes through [`sanitize_json`] first — the escaping
/// `serde_json` does keeps a control character from breaking the *document*,
/// but says nothing about what the reader sees once it has parsed it.
fn ok<T: serde::Serialize>(
    json_out: bool,
    report: &T,
    render: impl FnOnce(&T) -> String,
) -> CallOutcome {
    if json_out {
        return match serde_json::to_value(report) {
            Ok(mut value) => {
                sanitize_json(&mut value);
                CallOutcome::TaskOk {
                    text: value.to_string(),
                }
            }
            Err(e) => CallOutcome::ToolErr(format!("serialise report: {e}")),
        };
    }
    let text = render(report);
    let text = if text.starts_with(UNTRUSTED_FRAMING) {
        text
    } else {
        format!("{UNTRUSTED_FRAMING}{text}")
    };
    CallOutcome::TaskOk { text }
}

/// Replace the control characters in every string of `value` with spaces.
///
/// Graph content reaches a JSON reply in the **values**: paths, author names,
/// commit subjects, note text, quoted source lines. The keys are the report's
/// own field names, fixed in the Rust types the reports serialise from and in
/// the `sync` child's `--json` output, so they carry nothing an outsider wrote
/// and are left alone — rewriting a key could silently merge two of them.
///
/// Newline and tab survive; every other control character does not. That is
/// the one place this differs from [`repograph::sanitize`], and the reason is
/// what the two channels are. A digest is line-structured, so a newline inside
/// a value could forge a heading or an extra hit and has to go. A JSON value is
/// delimited by the grammar, so a newline inside one cannot escape it — and
/// some of these values *are* multi-line documents: `recall`'s report carries
/// the whole rendered digest, and `context` carries quoted source. Flattening
/// those would corrupt the report to defend against nothing. What is still
/// removed is everything that acts on a reader whatever contains it: escape
/// sequences, carriage returns that overwrite a line, backspace, `DEL`.
fn sanitize_json(value: &mut Js) {
    match value {
        Js::String(s) => {
            if s.chars().any(is_forbidden_control) {
                *s = s
                    .chars()
                    .map(|c| if is_forbidden_control(c) { ' ' } else { c })
                    .collect();
            }
        }
        Js::Array(items) => items.iter_mut().for_each(sanitize_json),
        Js::Object(map) => map.values_mut().for_each(sanitize_json),
        _ => {}
    }
}

/// A control character with no business in a JSON value: everything ASCII
/// control except the two that are ordinary text layout.
fn is_forbidden_control(c: char) -> bool {
    c.is_ascii_control() && c != '\n' && c != '\t'
}

/// An optional boolean argument. `Err` when present but wrong-typed.
fn bool_arg(args: &Js, name: &str) -> Result<bool, String> {
    match args.get(name) {
        None | Some(Js::Null) => Ok(false),
        Some(Js::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{name} must be a boolean")),
    }
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

// ── explore ──────────────────────────────────────────────────────────────────

/// Bytes an assistant's token is taken to be, for turning a `budget` in tokens
/// into one in bytes. Four is the usual English-and-code average, and the
/// budget is a ceiling rather than a measurement, so erring low would only
/// spend less than the caller allowed.
const BYTES_PER_TOKEN: usize = 4;
/// The default `budget`, in tokens: `DEFAULT_EXPLORE_BYTES` back in the unit a
/// caller thinks in, so the two cannot drift.
const DEFAULT_EXPLORE_TOKENS: u64 = (repograph::DEFAULT_EXPLORE_BYTES / BYTES_PER_TOKEN) as u64;
/// The smallest `budget` worth serving, matching the schema's `minimum`. Below
/// this a reply is a header and nothing else, so a smaller number is taken as
/// this one rather than as a request for silence.
const MIN_EXPLORE_TOKENS: u64 = 200;

fn tool_explore(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let target = match str_arg(args, "target") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let depth = match args.get("depth") {
        None | Some(Js::Null) => repograph::Depth::Context,
        Some(Js::String(s)) => match repograph::Depth::parse(s) {
            Some(d) => d,
            None => {
                return CallOutcome::ToolErr(format!(
                    "depth must be one of {}, got {s:?}",
                    repograph::Depth::NAMES.join(", ")
                ))
            }
        },
        Some(_) => return CallOutcome::ToolErr("depth must be a string".into()),
    };
    let tokens = match args.get("budget") {
        None | Some(Js::Null) => DEFAULT_EXPLORE_TOKENS,
        Some(v) => match v.as_u64() {
            Some(n) => n.max(MIN_EXPLORE_TOKENS),
            None => return CallOutcome::ToolErr("budget must be a positive integer".into()),
        },
    };
    let full = match bool_arg(args, "full") {
        Ok(b) => b,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let budget_bytes = usize::try_from(tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(BYTES_PER_TOKEN);
    // `None` for the repository, as `context` does: core-api falls back to the
    // `GitSync` marker, which is the checkout the store was built from.
    let report = {
        let g = db.read();
        repograph::explore(&*g, None, target, depth, full)
    };
    ok(json_out, &report, |r| {
        repograph::render_explore(r, budget_bytes)
    })
}

// ── map ──────────────────────────────────────────────────────────────────────

fn tool_map(db: &SharedDb, json_out: bool) -> CallOutcome {
    let map = {
        let g = db.read();
        repograph::repo_map(&*g, &MapOptions::default())
    };
    ok(json_out, &map, repograph::render_map)
}

// ── context ──────────────────────────────────────────────────────────────────

fn tool_context(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let target = match str_arg(args, "target") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let full = match bool_arg(args, "full") {
        Ok(b) => b,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    // `None` for the repository: core-api falls back to the `GitSync` marker,
    // which is the checkout the store was built from.
    let report = {
        let g = db.read();
        repograph::context_with(&*g, None, target, &ContextOptions { source: full })
    };
    ok(json_out, &report, repograph::render_context)
}

// ── impact ───────────────────────────────────────────────────────────────────

/// `project_dir` is the value of `$CLAUDE_PROJECT_DIR`, passed in rather than
/// read here so a test can exercise both branches of [`project_repo`] without
/// mutating the process environment.
fn tool_impact(
    db: &SharedDb,
    args: &Js,
    project_dir: Option<&OsStr>,
    json_out: bool,
) -> CallOutcome {
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
    ok(json_out, &report, repograph::render_impact)
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

fn tool_owners(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
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
    ok(json_out, &report, repograph::render_owners)
}

// ── why ──────────────────────────────────────────────────────────────────────

fn tool_why(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
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
    ok(json_out, &report, repograph::render_why)
}

// ── explain_association ──────────────────────────────────────────────────────

/// Why two entities are associated: every rule-derived edge between them, with
/// the rule that wrote it and the predicate it matched on.
///
/// The report is the same `Vec<Explanation>` the `explain` graph tool has
/// always returned — `json: true` hands it back unchanged. What is new is the
/// default: on an entity store this is the question the door exists for, and
/// an assistant asking it was getting a JSON array to parse where every other
/// question here answers in prose. `explain` is left as it was, for the caller
/// that wants the array without asking.
fn tool_explain_association(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let a = match str_arg(args, "a") {
        Ok(v) => v.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let b = match str_arg(args, "b") {
        Ok(v) => v.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    // Unlike `why`, a key the graph does not hold is an error here rather than
    // an `unknown:` line: `explain` resolves both keys to dense ids before it
    // looks at a single edge, and that is the engine's answer to give.
    let report = {
        let g = db.read();
        match g.explain(&a, &b) {
            Ok(v) => v,
            Err(e) => {
                return CallOutcome::ToolErr(match e {
                    GraphError::QueryError { detail } | GraphError::IngestError { detail } => {
                        detail
                    }
                    other => other.to_string(),
                })
            }
        }
    };
    ok(json_out, &report, |found| {
        render_explanations(&a, &b, found)
    })
}

/// One header, then one line per rule-derived edge, capped like every other
/// task digest.
///
/// Rule names, edge types and predicate fields are all graph content — a rule
/// is named by whoever created it — so each goes through
/// [`repograph::sanitize`] before it reaches a line-structured digest.
fn render_explanations(a: &str, b: &str, found: &[Explanation]) -> String {
    let mut out = format!(
        "mushroomdb explain — {} ↔ {}: {} relationship(s)\n",
        repograph::sanitize(a),
        repograph::sanitize(b),
        found.len()
    );
    if found.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for e in found {
        out.push_str(&format!(
            "  {} via rule {}",
            repograph::sanitize(&e.edge_type),
            repograph::sanitize(&e.rule)
        ));
        if let Some(weight) = e.weight {
            out.push_str(&format!(" (score {weight:.2})"));
        }
        if let Some(via) = &e.via_edge {
            out.push_str(&format!(" via {}", repograph::sanitize(via)));
        }
        out.push_str(&format!(" — {}\n", predicate_summary(&e.predicate)));
    }
    repograph::cap_lines(&out, repograph::MAX_TOOL_LINES)
}

/// A predicate in one clause: what it compares, on which fields, and the
/// threshold it had to clear.
fn predicate_summary(p: &PredicateSummary) -> String {
    let mut out = repograph::sanitize(&p.kind);
    if !p.fields.is_empty() {
        let fields: Vec<String> = p.fields.iter().map(|f| repograph::sanitize(f)).collect();
        out.push_str(&format!(" on {}", fields.join(", ")));
    }
    if let Some(min) = p.min {
        out.push_str(&format!(" >= {min}"));
    }
    if let Some(tolerance) = p.tolerance {
        out.push_str(&format!(" +/- {tolerance}"));
    }
    if let Some(km) = p.km {
        out.push_str(&format!(" within {km} km"));
    }
    if let Some(parts) = &p.parts {
        let inner: Vec<String> = parts.iter().map(predicate_summary).collect();
        out.push_str(&format!(" ({})", inner.join("; ")));
    }
    if p.approximate {
        out.push_str(" (approximate)");
    }
    out
}

// ── recall ───────────────────────────────────────────────────────────────────

fn tool_recall(db: &SharedDb, db_dir: Option<&Path>, args: &Js, json_out: bool) -> CallOutcome {
    let topic = match str_arg(args, "topic") {
        Ok(t) => t.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let label = db_dir.map_or_else(|| "store".to_string(), |d| d.display().to_string());
    // The topic goes in as the caller wrote it: `recall_digest` searches the
    // identifiers in it, and it is the same call the `recall` hook makes, so
    // the two cannot disagree about what a topic means.
    let digest = {
        let g = db.read();
        repograph::recall_digest(&*g, &topic, &label, MAX_OUTPUT_BYTES)
    };
    let text = if digest.is_empty() {
        format!(
            "mushroomdb recall — nothing indexed matches {}\n",
            repograph::sanitize(&topic)
        )
    } else {
        digest.clone()
    };
    ok(
        json_out,
        &json!({ "topic": topic, "digest": digest }),
        |_| text,
    )
}

// ── remember ─────────────────────────────────────────────────────────────────

fn tool_remember(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
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
                json_out,
                &json!({ "key": key, "kind": kind, "about": about }),
                |_| rendered,
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
fn tool_sync(db_dir: Option<&Path>, json_out: bool) -> CallOutcome {
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
    // The CLI already rendered the digest into the object, so the digest and
    // the numbers come from the one run whichever the caller asked for.
    let text = report
        .get("text")
        .and_then(Js::as_str)
        .unwrap_or_default()
        .to_string();
    ok(json_out, &Js::Object(report), |_| text)
}

// ── tools/list ───────────────────────────────────────────────────────────────

/// The `json` argument every task tool takes, added to all ten schemas by
/// [`task_tools`] rather than written out ten times.
fn json_arg() -> Js {
    json!({
        "type": "boolean",
        "description": "Answer with the report as JSON, not the rendered digest."
    })
}

/// The ten task tools, in the order `tools/list` puts them: the question an
/// assistant asks first comes first.
pub(crate) fn task_tools() -> Vec<Js> {
    let mut tools = task_tool_schemas();
    for tool in &mut tools {
        if let Some(props) = tool["inputSchema"]["properties"].as_object_mut() {
            props.insert("json".to_string(), json_arg());
        }
    }
    tools
}

fn task_tool_schemas() -> Vec<Js> {
    vec![
        json!({
            "name": "explore",
            "description": "Find your way around this repository from its code graph: a symbol's definition, callers and callees; the blast radius (files that import it or change with it) if it changes; who owns it and why files are related. Cheaper than grep for anything cross-file. depth=context (default) | impact | history | all.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A file path, a symbol key (path#name), or a bare symbol name."
                    },
                    "depth": {
                        "type": "string",
                        "enum": ["context", "impact", "history", "all"]
                    },
                    "budget": {
                        "type": "integer",
                        "minimum": 200,
                        "description": "Max reply tokens (default 1200)."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Include the source body."
                    }
                },
                "required": ["target"]
            }
        }),
        json!({
            "name": "map",
            "description": "Summarise the graphed repository in one screen: size, last sync, clusters, key files, owners, hot files, stale concepts, and questions worth asking next. Start here when you do not know the codebase.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "context",
            "description": "Everything known about one file or symbol: where it is as path:start-end, its signature and doc, owner, every call site into it grouped by calling file, its callees, importers and imports, co-change partners, recent commits, and any notes or concepts about it. The body is not quoted unless you ask for it with 'full'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A file path, a symbol key (path#name), or a bare symbol name. An ambiguous bare name returns the candidates instead."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Include the source body (default: pointers and signature only)."
                    }
                },
                "required": ["target"]
            }
        }),
        json!({
            "name": "impact",
            "description": "What else the files in a change reach: co-change partners, by similarity score or by how many commits the two share, plus importers, symbols used elsewhere, and each file's owner. Defaults to the current git diff plus untracked files when no list is given.",
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
            "description": "What links two files, symbols, or people, with the evidence for each link: shared commits, the importing line, every calling line, the file two authors both know. With no rule edge it reports the commits the two share, and failing that the shortest path between them.",
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
            "name": "explain_association",
            "description": "Why two entities are associated: every rule-derived edge between the two keys, with the rule that wrote it, its edge type, the match score, and the predicate it matched on. Both keys must already exist.",
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
            "description": "Where the graph says a topic lives: one pointer per hit — path:line, the symbol, and the first line of its doc — across notes, concepts, files, symbols and people.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Free-form text. The identifiers in it — a path, a `mod::name`, a snake_case word, or any word in backticks — are searched as phrases; a topic naming none of those matches nothing."
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

    /// The report behind a `json: true` reply, which is now the only place a
    /// caller reads the numbers from: the text content *is* the JSON.
    fn report(outcome: &CallOutcome) -> Js {
        match outcome {
            CallOutcome::TaskOk { text } => {
                serde_json::from_str(text).expect("a json reply is the serialised report")
            }
            other => panic!("expected a task result, got {}", describe(other)),
        }
    }

    fn impact_files(outcome: &CallOutcome) -> Vec<String> {
        report(outcome)["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|f| f["path"].as_str().expect("path").to_string())
            .collect()
    }

    /// `impact` with no `files`, asking for the report rather than the digest.
    fn impact_report(db: &SharedDb, project_dir: Option<&OsStr>) -> CallOutcome {
        tool_impact(db, &json!({"json": true}), project_dir, true)
    }

    fn describe(outcome: &CallOutcome) -> String {
        match outcome {
            CallOutcome::ToolErr(m) => format!("tool error: {m}"),
            CallOutcome::TaskOk { text } => format!("ok: {text}"),
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

        let outcome = impact_report(&db, None);
        assert_eq!(
            impact_files(&outcome),
            vec!["src/core.rs".to_string()],
            "the uncommitted edit, and not the build artefact"
        );
        assert_eq!(
            report(&outcome)["unknown"],
            json!([]),
            "an excluded path must not come back as unknown"
        );

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

        let outcome = impact_report(&db, Some(project.as_os_str()));
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

        let outcome = impact_report(&db, Some(repo.join("src").as_os_str()));
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

        let outcome = impact_report(&db, Some(plain.as_os_str()));
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

        let outcome = impact_report(
            &db,
            Some(OsStr::new("/nonexistent/mushroomdb-test-project")),
        );
        match &outcome {
            CallOutcome::ToolErr(m) => assert!(m.contains("pass files explicitly"), "{m}"),
            other => panic!("{}", describe(other)),
        }

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Binding: an explanation's line carries the score and the hop a via-rule
    /// went over, and the digest never runs past the line budget.
    ///
    /// `tests/mcp.rs` covers the plain rule and the empty case end to end; what
    /// is only reachable from here is a via-hop rule and a report longer than
    /// [`repograph::MAX_TOOL_LINES`], neither of which a two-node fixture
    /// produces.
    #[test]
    fn an_explanation_line_names_the_score_the_hop_and_the_predicate() {
        let one = |rule: &str, via: Option<&str>| Explanation {
            rule: rule.to_string(),
            edge_type: "SIMILAR".to_string(),
            src_key: "a".to_string(),
            dst_key: "b".to_string(),
            weight: Some(0.9625),
            predicate: PredicateSummary {
                kind: "vector_similar".to_string(),
                fields: vec!["emb".to_string()],
                min: Some(0.85),
                tolerance: None,
                km: None,
                parts: None,
                approximate: false,
            },
            via_edge: via.map(str::to_string),
        };

        let text = render_explanations("a", "b", &[one("close", Some("WORKS_AT"))]);
        assert_eq!(
            text,
            "mushroomdb explain — a ↔ b: 1 relationship(s)\n  SIMILAR via rule close (score 0.96) \
             via WORKS_AT — vector_similar on emb >= 0.85\n"
        );

        let many: Vec<Explanation> = (0..40).map(|i| one(&format!("r{i}"), None)).collect();
        let capped = render_explanations("a", "b", &many);
        assert_eq!(
            capped.lines().count(),
            repograph::MAX_TOOL_LINES,
            "the digest is capped like every other one"
        );
        assert!(
            capped.starts_with("mushroomdb explain — a ↔ b: 40 relationship(s)"),
            "and the header still says how many there were: {capped}"
        );
    }

    /// Binding: an explicit `files` list never looks at a repository at all.
    #[test]
    fn explicit_files_ignore_the_project_directory() {
        let (db, dir) = store_for("explicit-store", None);

        let outcome = tool_impact(
            &db,
            &json!({"files": ["src/core.rs"], "json": true}),
            Some(OsStr::new("/nonexistent/mushroomdb-test-project")),
            true,
        );
        assert_eq!(impact_files(&outcome), vec!["src/core.rs".to_string()]);

        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
