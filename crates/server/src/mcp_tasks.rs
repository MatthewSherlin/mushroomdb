//! The fourteen MCP tools that answer a question in prose rather than in JSON.
//!
//! `explore`, `map`, `context`, `impact`, `owners`, `why`, `recall`,
//! `remember` and `sync` sit in front of the thirteen graph tools in
//! `mcp::tools_list`, because they are what an assistant working in a checkout
//! actually reaches for: find me this thing, what is this repository, what is
//! this symbol, what does my diff touch, who wrote this, why are these two
//! linked, what do I already know, remember this, and bring the store up to
//! date. `explain_association` answers on a store with no repository in it:
//! why these two entities are associated, with the rule that derived each
//! edge.
//!
//! Four more answer the rest of the entity graph's questions, and they are the
//! ones the first association benchmark run showed an assistant failing to
//! find. `node_edges` and `neighborhood` used to hand back a JSON array of
//! `{edge_type, src_key, dst_key, derived}` — a listing with no rule, no score
//! and no evidence, which is why a run spent 195 `query` calls and 66
//! `edge_history` calls reconstructing what one reply could have said. Both
//! now answer in prose, grouped by edge type, each listed edge carrying the
//! rule that derived it, its score, and the predicate it matched on.
//! `edges_at` answers the same question at a past commit, and `what_if`
//! answers it about a change that has not been made.
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
use core_api::{json_to_value, Dir, Explanation, GraphError, PredicateSummary, SharedDb};
use serde_json::{json, Value as Js};
use std::collections::{BTreeMap, BTreeSet};
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

/// The fourteen names this module answers to. Listed once, so the `json`
/// argument below is read for exactly the tools that declare it.
///
/// `explore` comes first because it is the whole default surface of a
/// code-graph store: the one tool a session finds, composed from the three
/// beneath it. `explain_association` sits beside `why` because they are the
/// same question asked of the two doors: what links these two, with the
/// evidence — `why` from a code graph, `explain_association` from the rules
/// that derived the edge. The four entity tools follow it, because they are
/// the same question widened: every relationship of one node rather than of
/// one pair, that listing at a past commit, and that listing under a change
/// that has not been made.
pub(crate) const TASK_TOOLS: [&str; 14] = [
    "explore",
    "map",
    "context",
    "impact",
    "owners",
    "why",
    "explain_association",
    "node_edges",
    "neighborhood",
    "edges_at",
    "what_if",
    "recall",
    "remember",
    "sync",
];

/// Route a task tool. `None` when `name` is not one of the fourteen.
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
        "node_edges" => tool_node_edges(db, args, json_out),
        "neighborhood" => tool_neighborhood(db, args, json_out),
        "edges_at" => tool_edges_at(db, args, json_out),
        "what_if" => tool_what_if(db, db_dir, args, json_out),
        "recall" => tool_recall(db, db_dir, args, json_out),
        "remember" => tool_remember(db, args, json_out),
        "sync" => tool_sync(db_dir, json_out),
        _ => unreachable!("TASK_TOOLS and this match list the same fourteen names"),
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

/// An optional string argument. `Err` when present but not a non-empty string.
///
/// An empty string is a filter that matches nothing, which no caller means —
/// they mean "no filter" — so it is refused rather than answered with zero
/// edges.
fn opt_str_arg<'a>(args: &'a Js, name: &str) -> Result<Option<&'a str>, String> {
    match args.get(name) {
        None | Some(Js::Null) => Ok(None),
        Some(Js::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{name} must be a non-empty string")),
    }
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
            Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
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

// ── node_edges / neighborhood ────────────────────────────────────────────────

/// Edges listed per edge type when the caller names no `limit`, and the
/// number of `explain` calls one type may cost.
const DEFAULT_EDGE_LIMIT: usize = 10;

/// The largest `limit` a caller may ask for.
///
/// The cost of this reply is one `explain` call per listed partner that is
/// joined by a derived edge — about a millisecond each — so the cap on what is
/// listed is also the cap on what the call costs. A hub node has thousands of
/// partners; a reply is a screen, not a dump.
const MAX_EDGE_LIMIT: usize = 100;

/// Longest edge digest, in lines. Wider than [`repograph::MAX_TOOL_LINES`]
/// because this listing is the reply an assistant reads instead of calling
/// `query` twenty times, and a default `limit` over four edge types already
/// runs past twenty-five lines. The header counts every edge whatever is
/// printed, so a capped digest still says how much it is not showing.
const MAX_EDGE_LINES: usize = repograph::MAX_MAP_LINES;

/// One incident edge, with whatever the rules say about it.
///
/// `rule`, `score` and `predicate` are `Some` only for a derived edge that
/// `explain` accounted for: a manual edge was written by a caller, not
/// matched by a predicate, and has nothing to explain.
struct EdgeLine {
    edge_type: String,
    /// The node at the other end. For a self-loop, the node itself.
    other: String,
    /// `true` when the edge runs out of the node asked about.
    outgoing: bool,
    derived: bool,
    rule: Option<String>,
    score: Option<f64>,
    predicate: Option<String>,
}

/// Every edge of one type incident on the node, and the slice of them listed.
struct EdgeGroup {
    edge_type: String,
    /// How many edges of this type the node has, before the `limit`.
    count: usize,
    listed: Vec<EdgeLine>,
}

/// The edges incident on `key`, grouped by edge type, with each listed edge
/// attributed to the rule that derived it.
///
/// `types` and `dir` are the `neighborhood` filters; `node_edges` passes its
/// single `edge_type` as a one-element list and [`Dir::Both`].
///
/// # What this costs
///
/// One `explain(key, other)` call per **distinct partner** among the listed
/// edges that carries a derived edge, memoised across types so a partner
/// joined by three rules costs one call rather than three. Nothing outside the
/// listed slice is explained, so the bound is `limit` partners per edge type.
///
/// That bound is also the one honest limit on the ordering: a score is only
/// known for an edge that was explained, so a type with more than `limit`
/// edges lists the first `limit` the engine returns and orders **those** by
/// score. Ordering all of them by score would mean explaining all of them,
/// which is the cost this cap exists to refuse.
fn node_edge_groups(
    db: &SharedDb,
    key: &str,
    types: Option<&[String]>,
    dir: Dir,
    limit: usize,
) -> Result<(usize, Vec<EdgeGroup>), GraphError> {
    let edges = {
        let g = db.read();
        g.node_edges(key)?
    };

    let mut by_type: BTreeMap<String, Vec<(String, bool, bool)>> = BTreeMap::new();
    let mut total = 0usize;
    for e in edges {
        if let Some(wanted) = types {
            if !wanted.iter().any(|t| t == &e.edge_type) {
                continue;
            }
        }
        let outgoing = e.src_key == key;
        match dir {
            Dir::Out if !outgoing => continue,
            Dir::In if outgoing => continue,
            _ => {}
        }
        let other = if outgoing {
            e.dst_key.clone()
        } else {
            e.src_key.clone()
        };
        total += 1;
        by_type
            .entry(e.edge_type.clone())
            .or_default()
            .push((other, outgoing, e.derived));
    }

    // Memoised per partner, not per edge: `explain` answers for every rule
    // edge between the pair at once, whatever its type.
    let mut explained: BTreeMap<String, Vec<Explanation>> = BTreeMap::new();
    let mut groups = Vec::with_capacity(by_type.len());
    for (edge_type, rows) in by_type {
        let count = rows.len();
        let mut listed: Vec<EdgeLine> = Vec::with_capacity(count.min(limit));
        for (other, outgoing, derived) in rows.into_iter().take(limit) {
            let mut line = EdgeLine {
                edge_type: edge_type.clone(),
                other,
                outgoing,
                derived,
                rule: None,
                score: None,
                predicate: None,
            };
            if derived {
                if !explained.contains_key(&line.other) {
                    let found = {
                        let g = db.read();
                        g.explain(key, &line.other).unwrap_or_default()
                    };
                    explained.insert(line.other.clone(), found);
                }
                let found = explained.get(&line.other).map(Vec::as_slice).unwrap_or(&[]);
                if let Some(e) = found.iter().find(|e| {
                    e.edge_type == edge_type
                        && if outgoing {
                            e.src_key == key && e.dst_key == line.other
                        } else {
                            e.src_key == line.other && e.dst_key == key
                        }
                }) {
                    line.rule = Some(e.rule.clone());
                    line.score = e.weight;
                    line.predicate = Some(predicate_summary(&e.predicate));
                }
            }
            listed.push(line);
        }
        // Strongest first. An edge with no score — a manual one, or a rule
        // that declares no `weight_prop` — sorts last rather than pretending
        // to a score of zero, and ties break on the partner key so the reply
        // is byte-stable.
        listed.sort_by(|a, b| {
            let sa = a.score.unwrap_or(f64::NEG_INFINITY);
            let sb = b.score.unwrap_or(f64::NEG_INFINITY);
            sb.partial_cmp(&sa)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.other.cmp(&b.other))
        });
        groups.push(EdgeGroup {
            edge_type,
            count,
            listed,
        });
    }
    Ok((total, groups))
}

/// One header, then one block per edge type: its name, how many edges the node
/// has of it, and the listed ones with their direction, rule, score and
/// predicate.
///
/// Edge types, partner keys, rule names and predicate fields are all graph
/// content, so every one of them goes through [`repograph::sanitize`] before
/// it reaches a line-structured digest.
fn render_edge_groups(key: &str, total: usize, groups: &[EdgeGroup]) -> String {
    let mut out = format!(
        "mushroomdb edges — {}: {total} edge(s) over {} type(s)\n",
        repograph::sanitize(key),
        groups.len()
    );
    if groups.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for g in groups {
        out.push_str(&format!(
            "{} ({})\n",
            repograph::sanitize(&g.edge_type),
            g.count
        ));
        for e in &g.listed {
            let arrow = if e.outgoing { "→" } else { "←" };
            out.push_str(&format!("  {arrow} {}", repograph::sanitize(&e.other)));
            if let Some(rule) = &e.rule {
                out.push_str(&format!("  rule {}", repograph::sanitize(rule)));
            }
            if let Some(score) = e.score {
                out.push_str(&format!("  score {score:.2}"));
            }
            if let Some(predicate) = &e.predicate {
                out.push_str(&format!(" — {predicate}"));
            }
            out.push('\n');
        }
        if g.count > g.listed.len() {
            out.push_str(&format!("  … and {} more\n", g.count - g.listed.len()));
        }
    }
    repograph::cap_lines(&out, MAX_EDGE_LINES)
}

/// The same grouping as a document, for `json: true`.
fn edge_groups_json(key: &str, total: usize, groups: &[EdgeGroup]) -> Js {
    json!({
        "key": key,
        "total": total,
        "types": groups.iter().map(|g| json!({
            "edge_type": g.edge_type,
            "count": g.count,
            "listed": g.listed.len(),
            "edges": g.listed.iter().map(edge_line_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn edge_line_json(e: &EdgeLine) -> Js {
    json!({
        "edge_type": e.edge_type,
        "other": e.other,
        "direction": if e.outgoing { "out" } else { "in" },
        "derived": e.derived,
        "rule": e.rule,
        "score": e.score,
        "predicate": e.predicate,
    })
}

/// The `limit` argument: how many edges of each type to list.
fn edge_limit_arg(args: &Js) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Js::Null) => Ok(DEFAULT_EDGE_LIMIT),
        Some(v) => match v.as_u64() {
            // Zero is a reply with counts and no edges, which no caller means.
            Some(0) | None => Err("limit must be a positive integer".into()),
            Some(n) => Ok(usize::try_from(n)
                .unwrap_or(MAX_EDGE_LIMIT)
                .min(MAX_EDGE_LIMIT)),
        },
    }
}

fn tool_node_edges(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let key = match str_arg(args, "key") {
        Ok(k) => k,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let edge_type = match opt_str_arg(args, "edge_type") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let limit = match edge_limit_arg(args) {
        Ok(n) => n,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let filter = edge_type.map(|t| vec![t.to_string()]);
    edge_reply(db, key, filter.as_deref(), Dir::Both, limit, json_out)
}

/// Group, render and answer — the tail both `node_edges` and a depth-1
/// `neighborhood` share.
fn edge_reply(
    db: &SharedDb,
    key: &str,
    types: Option<&[String]>,
    dir: Dir,
    limit: usize,
    json_out: bool,
) -> CallOutcome {
    match node_edge_groups(db, key, types, dir, limit) {
        Ok((total, groups)) => ok(json_out, &edge_groups_json(key, total, &groups), |_| {
            render_edge_groups(key, total, &groups)
        }),
        Err(e) => CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
    }
}

/// One hop is the edge listing; further than that is still the BFS table.
///
/// Depth 1 is the question this tool is nearly always asked — what is this
/// node joined to — and a table of `(key, label, depth)` answers it without
/// saying *why* any row is there. Past one hop there is no single rule behind
/// a row, so the table is still the honest shape and is returned unchanged.
fn tool_neighborhood(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let key = match str_arg(args, "key") {
        Ok(k) => k,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let depth = match args.get("depth") {
        None | Some(Js::Null) => 1u32,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(d) => d,
            None => return CallOutcome::ToolErr("depth must be an integer".into()),
        },
    };
    let dir = match args.get("direction") {
        None | Some(Js::Null) => Dir::Both,
        Some(v) => match v.as_str() {
            Some(s) if s.eq_ignore_ascii_case("out") => Dir::Out,
            Some(s) if s.eq_ignore_ascii_case("in") => Dir::In,
            Some(s) if s.eq_ignore_ascii_case("both") => Dir::Both,
            Some(other) => return CallOutcome::ToolErr(format!("unknown direction: {other}")),
            None => return CallOutcome::ToolErr("direction must be a string".into()),
        },
    };
    let edge_types = match str_list_arg(args, "edge_types") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let filter = (!edge_types.is_empty()).then_some(edge_types.as_slice());

    if depth <= 1 {
        let limit = match edge_limit_arg(args) {
            Ok(n) => n,
            Err(e) => return CallOutcome::ToolErr(e),
        };
        return edge_reply(db, key, filter, dir, limit, json_out);
    }

    let etype_refs: Option<Vec<&str>> = filter.map(|v| v.iter().map(String::as_str).collect());
    let rs = {
        let g = db.read();
        match g.node_ref(key) {
            Some(n) => Ok(n.neighborhood(depth, etype_refs.as_deref(), dir)),
            None => Err(GraphError::KeyNotFound {
                key: key.to_string(),
            }),
        }
    };
    match rs {
        Ok(rs) => CallOutcome::ToolOk(crate::json::result_set_json(&rs)),
        Err(e) => CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
    }
}

// ── edges_at ─────────────────────────────────────────────────────────────────

fn tool_edges_at(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let key = match str_arg(args, "key") {
        Ok(k) => k,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let at = match args.get("at") {
        None | Some(Js::Null) => return CallOutcome::ToolErr("missing at".into()),
        Some(v) => match v.as_u64() {
            Some(n) => n,
            None => return CallOutcome::ToolErr("at must be a non-negative integer".into()),
        },
    };
    engine_edges_at(db, key, at, json_out)
}

/// The point-in-time edge listing.
///
/// The engine call this needs is being added on another branch, so the
/// dispatch arm, the schema and the argument checks above are in place and the
/// answer is not: a caller is told the build cannot serve it rather than being
/// handed a listing from the wrong commit.
// wired to GraphDb::edges_at when the engine stream merges
fn engine_edges_at(_db: &SharedDb, _key: &str, _at: u64, _json_out: bool) -> CallOutcome {
    CallOutcome::ToolErr("edges_at is not available in this build".into())
}

// ── what_if ──────────────────────────────────────────────────────────────────

/// One edge the change would lose or gain.
struct DiffEdge {
    edge_type: String,
    other: String,
    outgoing: bool,
    derived: bool,
    rule: Option<String>,
    score: Option<f64>,
}

/// The identity of an incident edge: type, partner, direction. Two edges with
/// the same triple are the same edge, so this is what a diff compares.
type EdgeId = (String, String, bool);

/// What a change to one property would do to one node's relationships.
///
/// The change is never applied to the live store. The store directory is
/// copied to a fresh temp directory, the copy is opened read-write, the
/// property is set there, and the node's edges before and after are diffed;
/// the copy is deleted whichever way that goes. A copy of 166 MiB takes about
/// 0.2 s, which is the price of answering "what would happen" without it
/// having happened.
///
/// The rules behind the **gained** edges are read from the copy, which is the
/// only place they exist. The rules behind the **lost** ones are read from the
/// live store, whose state is the copy's "before" — asked for after the diff
/// is known, so only the partners that actually changed cost a call.
fn tool_what_if(db: &SharedDb, db_dir: Option<&Path>, args: &Js, json_out: bool) -> CallOutcome {
    let key = match str_arg(args, "key") {
        Ok(k) => k.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let field = match str_arg(args, "field") {
        Ok(f) => f.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let Some(raw) = args.get("value").filter(|v| !v.is_null()) else {
        return CallOutcome::ToolErr("missing value".into());
    };
    let Some(value) = json_to_value(raw.clone()) else {
        return CallOutcome::ToolErr(format!(
            "value is not a supported value type: {}",
            repograph::sanitize(&raw.to_string())
        ));
    };
    let Some(db_dir) = db_dir else {
        return CallOutcome::ToolErr(
            "store path unknown: what_if needs the directory this server was started on".into(),
        );
    };
    // Checked before a byte is copied: a mistyped key is the likeliest way to
    // call this, and it is not worth 0.2 s to find out.
    {
        let g = db.read();
        if !g.has_node(&key) {
            return CallOutcome::ToolErr(crate::mcp::graph_err_msg(GraphError::KeyNotFound {
                key: key.clone(),
            }));
        }
    }

    let copy_dir = what_if_dir();
    let diffed = what_if_on_copy(db_dir, &copy_dir, &key, &field, value);
    let _ = std::fs::remove_dir_all(&copy_dir);
    let (lost_ids, gained) = match diffed {
        Ok(v) => v,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    // The live store still holds the state the copy started from, so it is
    // what says which rule wrote an edge the change would remove.
    let mut lost: Vec<DiffEdge> = Vec::with_capacity(lost_ids.len());
    let mut explained: BTreeMap<String, Vec<Explanation>> = BTreeMap::new();
    for ((edge_type, other, outgoing), derived) in lost_ids {
        let mut edge = DiffEdge {
            edge_type,
            other,
            outgoing,
            derived,
            rule: None,
            score: None,
        };
        if derived {
            if !explained.contains_key(&edge.other) {
                let found = {
                    let g = db.read();
                    g.explain(&key, &edge.other).unwrap_or_default()
                };
                explained.insert(edge.other.clone(), found);
            }
            let found = explained.get(&edge.other).map(Vec::as_slice).unwrap_or(&[]);
            if let Some(e) = found
                .iter()
                .find(|e| e.edge_type == edge.edge_type && endpoints_match(e, &key, &edge))
            {
                edge.rule = Some(e.rule.clone());
                edge.score = e.weight;
            }
        }
        lost.push(edge);
    }

    let report = json!({
        "key": key,
        "field": field,
        "value": raw.clone(),
        "lost": lost.iter().map(diff_edge_json).collect::<Vec<_>>(),
        "gained": gained.iter().map(diff_edge_json).collect::<Vec<_>>(),
    });
    ok(json_out, &report, |_| {
        render_what_if(&key, &field, raw, &lost, &gained)
    })
}

fn endpoints_match(e: &Explanation, key: &str, edge: &DiffEdge) -> bool {
    if edge.outgoing {
        e.src_key == key && e.dst_key == edge.other
    } else {
        e.src_key == edge.other && e.dst_key == key
    }
}

/// A fresh directory for one `what_if` copy: this process, this call.
fn what_if_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mushroomdb-what-if-{}-{n}", std::process::id()))
}

/// Copy, open, set, diff. Returns the lost edges as identities (their rules
/// are read from the live store afterwards) and the gained ones already
/// attributed, since the copy is the only place their rules exist.
#[allow(clippy::type_complexity)]
fn what_if_on_copy(
    src: &Path,
    copy_dir: &Path,
    key: &str,
    field: &str,
    value: core_api::Value,
) -> Result<(Vec<(EdgeId, bool)>, Vec<DiffEdge>), String> {
    copy_dir_all(src, copy_dir).map_err(|e| format!("what_if could not copy the store: {e}"))?;
    let copy = SharedDb::open(copy_dir).map_err(|e| {
        format!(
            "what_if could not open the copy: {}",
            crate::mcp::graph_err_msg(e)
        )
    })?;

    let before = incident_edges(&copy, key)?;
    {
        let mut g = copy.write();
        g.set_prop(key, field, value)
            .map_err(crate::mcp::graph_err_msg)?;
    }
    let after = incident_edges(&copy, key)?;

    let lost: Vec<(EdgeId, bool)> = before
        .iter()
        .filter(|(id, _)| !after.contains_key(*id))
        .map(|(id, derived)| (id.clone(), *derived))
        .collect();

    let mut gained: Vec<DiffEdge> = Vec::new();
    let mut explained: BTreeMap<String, Vec<Explanation>> = BTreeMap::new();
    for ((edge_type, other, outgoing), derived) in &after {
        if before.contains_key(&(edge_type.clone(), other.clone(), *outgoing)) {
            continue;
        }
        let mut edge = DiffEdge {
            edge_type: edge_type.clone(),
            other: other.clone(),
            outgoing: *outgoing,
            derived: *derived,
            rule: None,
            score: None,
        };
        if *derived {
            if !explained.contains_key(other) {
                let found = {
                    let g = copy.read();
                    g.explain(key, other).unwrap_or_default()
                };
                explained.insert(other.clone(), found);
            }
            let found = explained.get(other).map(Vec::as_slice).unwrap_or(&[]);
            if let Some(e) = found
                .iter()
                .find(|e| &e.edge_type == edge_type && endpoints_match(e, key, &edge))
            {
                edge.rule = Some(e.rule.clone());
                edge.score = e.weight;
            }
        }
        gained.push(edge);
    }

    // The handle owns a drain thread and the copy's files; it has to be gone
    // before the caller deletes the directory under it.
    drop(copy);
    Ok((lost, gained))
}

/// Every edge incident on `key`, keyed by identity, with its derived flag.
fn incident_edges(db: &SharedDb, key: &str) -> Result<BTreeMap<EdgeId, bool>, String> {
    let edges = {
        let g = db.read();
        g.node_edges(key).map_err(crate::mcp::graph_err_msg)?
    };
    let mut out = BTreeMap::new();
    for e in edges {
        let outgoing = e.src_key == key;
        let other = if outgoing { e.dst_key } else { e.src_key };
        out.insert((e.edge_type, other, outgoing), e.derived);
    }
    Ok(out)
}

/// Copy a directory tree. Symlinks are skipped: a store is a directory of
/// plain files, and following one would copy something outside it.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn diff_edge_json(e: &DiffEdge) -> Js {
    json!({
        "edge_type": e.edge_type,
        "other": e.other,
        "direction": if e.outgoing { "out" } else { "in" },
        "derived": e.derived,
        "rule": e.rule,
        "score": e.score,
    })
}

fn render_what_if(
    key: &str,
    field: &str,
    value: &Js,
    lost: &[DiffEdge],
    gained: &[DiffEdge],
) -> String {
    let mut out = format!(
        "mushroomdb what_if — {}.{} = {}: {} lost, {} gained\n",
        repograph::sanitize(key),
        repograph::sanitize(field),
        repograph::sanitize(&value.to_string()),
        lost.len(),
        gained.len()
    );
    for (heading, edges) in [("lost", lost), ("gained", gained)] {
        out.push_str(&format!("{heading} ({})\n", edges.len()));
        if edges.is_empty() {
            out.push_str("  none\n");
            continue;
        }
        for e in edges {
            let arrow = if e.outgoing { "→" } else { "←" };
            out.push_str(&format!(
                "  {arrow} {} {}",
                repograph::sanitize(&e.edge_type),
                repograph::sanitize(&e.other)
            ));
            if let Some(rule) = &e.rule {
                out.push_str(&format!("  rule {}", repograph::sanitize(rule)));
            }
            if let Some(score) = e.score {
                out.push_str(&format!("  score {score:.2}"));
            }
            out.push('\n');
        }
    }
    repograph::cap_lines(&out, MAX_EDGE_LINES)
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
            "description": "Why are A and B related — every relationship between the two keys and its evidence: the rule that derived it, its edge type, the match score, and the predicate it matched on. Both keys must already exist.",
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
            "name": "node_edges",
            "description": "What is K related to — every relationship of one node, grouped by edge type with a count, each listed edge carrying its direction, the rule that derived it, its score and the predicate it matched on. Answers 'why is this here' in the same call that lists it, so no follow-up explain is needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key." },
                    "edge_type": {
                        "type": "string",
                        "minLength": 1,
                        "description": "List only edges of this type. Omit for every type."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "Edges listed per edge type (default 10). The rest are counted as '… and N more'."
                    }
                },
                "required": ["key"]
            }
        }),
        json!({
            "name": "neighborhood",
            "description": "What is around K — one hop out, as the same grouped relationship listing node_edges gives, with the rule and score behind each edge. With depth above 1 it is the breadth-first table of (key, label, depth) instead, because past one hop no single rule accounts for a row.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key to start from." },
                    "depth": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Hops to traverse (default 1). 1 gives the relationship listing; above 1 gives the traversal table."
                    },
                    "edge_types": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Only follow these edge types. Omit for every type."
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["out", "in", "both"],
                        "description": "Edge direction to follow (default both)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "At depth 1, edges listed per edge type (default 10)."
                    }
                },
                "required": ["key"]
            }
        }),
        json!({
            "name": "edges_at",
            "description": "What did K's relationships look like at commit C — the edges that were live at one point in the store's history, with the rule that had derived each. Use this instead of replaying node_history or edge_history by hand.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key." },
                    "at": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "0-based WAL commit index to read the edges at."
                    }
                },
                "required": ["key", "at"]
            }
        }),
        json!({
            "name": "what_if",
            "description": "What changes if K's FIELD became VALUE — the relationships lost and gained, with the rule behind each. Nothing is written: the store is copied, the change is made on the copy, the copy is diffed and deleted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key to change." },
                    "field": { "type": "string", "minLength": 1, "description": "Property name to set." },
                    "value": {
                        "description": "The value it would take: a string, number, boolean, or a list or map of those. Not null."
                    }
                },
                "required": ["key", "field", "value"]
            }
        }),
        json!({
            "name": "recall",
            "description": "What do I already know about this — where the graph says a topic lives: one pointer per hit, path:line, the symbol, and the first line of its doc, across notes, concepts, files, symbols and people.",
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
            "description": "Remember this for next time — write a note into the graph and return its key. Keys listed in 'about' are linked to the note, and every one of them must already exist.",
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
