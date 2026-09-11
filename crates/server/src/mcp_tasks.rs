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
use core_api::{
    json_to_value, Dir, Explanation, GraphError, NodeInfo, PredicateSummary, SharedDb, Value,
};
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
        "what_if" => tool_what_if(db, args, json_out),
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
        let found = match g.explain(&a, &b) {
            Ok(v) => v,
            Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
        };
        // The matched values are read here, under the same read guard the
        // edges came from, so the evidence cannot describe a graph that has
        // since moved.
        found
            .into_iter()
            .map(|e| {
                // A via-hop rule evaluates its predicate between the *via*
                // node and the destination, not between the two keys the
                // caller asked about, so there is no pair of nodes here whose
                // values would be the evidence — the line still names the hop.
                let evidence = if e.via_edge.is_some() {
                    None
                } else {
                    match (g.node_info(&e.src_key), g.node_info(&e.dst_key)) {
                        (Some(src), Some(dst)) => {
                            predicate_evidence(&e.predicate, &src, &dst, e.weight)
                        }
                        _ => None,
                    }
                };
                ExplainedEdge { edge: e, evidence }
            })
            .collect::<Vec<_>>()
    };
    ok(json_out, &report, |found| {
        render_explanations(&a, &b, found)
    })
}

/// One explained edge, with the values that made the predicate true.
///
/// The `Explanation` fields are flattened, so `json: true` hands back the
/// array it always did with one `evidence` object added per relationship.
#[derive(serde::Serialize)]
struct ExplainedEdge {
    #[serde(flatten)]
    edge: Explanation,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<Evidence>,
}

/// What the two nodes actually had in common, per predicate kind.
///
/// Naming the rule and the threshold was never the answer to "why are these
/// two related" — the shared values are. Without them an assistant fetches
/// both nodes' raw property lists and reads them out, which names every
/// value either node holds rather than the ones they share.
#[derive(serde::Serialize)]
#[serde(untagged)]
enum Evidence {
    /// `overlap` — the intersection of the two lists, sorted.
    Shared { field: String, shared: Vec<Js> },
    /// `field_equal` / `key_match` — the one value both carry.
    Value { field: String, value: Js },
    /// `geo_radius` — both points and the distance between them.
    Geo {
        field: String,
        a: Js,
        b: Js,
        km: f64,
    },
    /// `numeric_within` / `vector_similar` — the two sides. For
    /// `vector_similar` the vectors themselves are useless to read, so `a`
    /// and `b` are omitted and only the score stands.
    Pair {
        field: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        a: Option<Js>,
        #[serde(skip_serializing_if = "Option::is_none")]
        b: Option<Js>,
        #[serde(skip_serializing_if = "Option::is_none")]
        similarity: Option<f64>,
    },
    /// `all` / `any` — one entry per branch that contributed.
    Parts { parts: Vec<Evidence> },
}

/// Mean Earth radius, as the rules engine uses for `geo_radius`.
const EARTH_RADIUS_KM: f64 = 6371.0088;

/// Evidence for one predicate, recursing through `all` / `any`.
///
/// `score` is the edge's weight and is passed only at the top level: `all`
/// takes the minimum of its branches and `any` the maximum, so a branch's own
/// score is not recoverable from the edge and a nested `vector_similar` has
/// no similarity to report.
///
/// # Only what matched
///
/// A branch reports evidence **only when that branch is itself satisfied**,
/// thresholds applied: an `overlap` under its `min`, a `numeric_within` past
/// its `tolerance`, a `geo_radius` past its `km`, a `key_match` whose field
/// does not name the other node. Under `any` that is the whole point — one
/// branch carries the edge and the others did not — and printing an unmatched
/// branch stated a reason the engine had rejected (`size_bucket: 1 vs 9` on a
/// `±2` tolerance). Under `all` every branch matched by construction, so the
/// checks change nothing there.
fn predicate_evidence(
    p: &PredicateSummary,
    src: &NodeInfo,
    dst: &NodeInfo,
    score: Option<f64>,
) -> Option<Evidence> {
    if let Some(parts) = &p.parts {
        let parts: Vec<Evidence> = parts
            .iter()
            .filter_map(|q| predicate_evidence(q, src, dst, None))
            .collect();
        return (!parts.is_empty()).then_some(Evidence::Parts { parts });
    }
    let field = p.fields.first()?.clone();
    match p.kind.as_str() {
        "overlap" => {
            let (Some(Value::List(a)), Some(Value::List(b))) =
                (src.props.get(&field), dst.props.get(&field))
            else {
                return None;
            };
            // Compared as the rules engine compares them: only a scalar
            // element is a token, and its type is part of its identity, so a
            // `1` and a `1.0` in two lists are not an overlap.
            let left: BTreeSet<(u8, String)> = a.iter().filter_map(scalar_token).collect();
            let right: BTreeSet<(u8, String)> = b.iter().filter_map(scalar_token).collect();
            let union = left.union(&right).count();
            let mut shared: Vec<String> = left
                .intersection(&right)
                .map(|(_, text)| text.clone())
                .collect();
            shared.sort();
            shared.dedup();
            if shared.is_empty() || union == 0 {
                return None;
            }
            // The rule's own test: the Jaccard ratio, against the `min` the
            // predicate declares. Inside an `any`, a list that overlaps but
            // not enough is a branch the engine rejected.
            let jaccard = shared.len() as f64 / union as f64;
            if p.min.is_some_and(|min| jaccard < min) {
                return None;
            }
            Some(Evidence::Shared {
                field,
                shared: shared.into_iter().map(Js::String).collect(),
            })
        }
        "field_equal" => {
            let v = src.props.get(&field)?;
            (dst.props.get(&field) == Some(v)).then(|| Evidence::Value {
                field,
                value: crate::json::value_to_json(v),
            })
        }
        // A key-match rule reads a foreign key off the source; the value they
        // share is the destination's own key — when the field really does name
        // it, directly or as one element of a list of foreign keys.
        "key_match" => {
            let names_dst = match src.props.get(&field)? {
                Value::Str(s) => s == &dst.key,
                Value::List(items) => items
                    .iter()
                    .any(|v| matches!(v, Value::Str(s) if s == &dst.key)),
                _ => false,
            };
            names_dst.then(|| Evidence::Value {
                field,
                value: Js::String(dst.key.clone()),
            })
        }
        "numeric_within" => {
            let (a, b) = (src.props.get(&field)?, dst.props.get(&field)?);
            let (x, y) = (numeric(a)?, numeric(b)?);
            let delta = (x - y).abs();
            // The rule's own test. A zero tolerance asks for equality.
            let within = match p.tolerance {
                Some(0.0) => delta == 0.0,
                Some(t) => delta <= t,
                None => true,
            };
            within.then(|| Evidence::Pair {
                field,
                a: Some(crate::json::value_to_json(a)),
                b: Some(crate::json::value_to_json(b)),
                similarity: None,
            })
        }
        "geo_radius" => {
            let (alat, alon) = lat_lon(src.props.get(&field)?)?;
            let (blat, blon) = lat_lon(dst.props.get(&field)?)?;
            let km = haversine_km(alat, alon, blat, blon);
            if p.km.is_some_and(|radius| km > radius) {
                return None;
            }
            Some(Evidence::Geo {
                field,
                a: Js::String(format_lat_lon(alat, alon)),
                b: Js::String(format_lat_lon(blat, blon)),
                km: round2(km),
            })
        }
        // The two vectors say nothing a reader can use; the cosine the rule
        // scored does, and that is the edge's weight.
        "vector_similar" => score.map(|sim| Evidence::Pair {
            field,
            a: None,
            b: None,
            similarity: Some(sim),
        }),
        _ => None,
    }
}

/// The comparable token of one list element, as `(type tag, text)`.
///
/// Mirrors `ValueKey::from_value`: a nested list or map is not a token and
/// cannot overlap, and two tokens of different types never match however
/// alike they read.
fn scalar_token(v: &Value) -> Option<(u8, String)> {
    match v {
        Value::Str(s) => Some((0, s.clone())),
        Value::Int(i) => Some((1, i.to_string())),
        Value::Float(f) => Some((2, format!("{f}"))),
        Value::Bool(b) => Some((3, b.to_string())),
        Value::List(_) | Value::Map(_) => None,
    }
}

/// A `[lat, lon]` pair, as `geo_radius` reads it.
fn lat_lon(v: &Value) -> Option<(f64, f64)> {
    let Value::List(items) = v else {
        return None;
    };
    if items.len() != 2 {
        return None;
    }
    Some((numeric(&items[0])?, numeric(&items[1])?))
}

/// A finite number, as the rules engine reads one: an integer or a finite
/// float, and nothing else.
fn numeric(v: &Value) -> Option<f64> {
    match v {
        #[allow(clippy::cast_precision_loss)]
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    }
}

fn format_lat_lon(lat: f64, lon: f64) -> String {
    format!("{:.4},{:.4}", lat, lon)
}

fn round2(km: f64) -> f64 {
    (km * 100.0).round() / 100.0
}

/// Great-circle distance in km — the same formula `geo_radius` scores with,
/// so the printed distance and the edge's score agree.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = ((dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2))
        .clamp(0.0, 1.0);
    EARTH_RADIUS_KM * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// One evidence clause, rendered for the digest line.
fn evidence_summary(e: &Evidence) -> String {
    match e {
        Evidence::Shared { field, shared } => {
            let vals: Vec<String> = shared.iter().map(json_scalar_text).collect();
            format!("{}: {}", repograph::sanitize(field), vals.join(", "))
        }
        Evidence::Value { field, value } => format!(
            "{}: {}",
            repograph::sanitize(field),
            json_scalar_text(value)
        ),
        Evidence::Geo { field, a, b, km } => format!(
            "{}: {} vs {}, {km} km apart",
            repograph::sanitize(field),
            json_scalar_text(a),
            json_scalar_text(b)
        ),
        Evidence::Pair {
            field,
            a: Some(a),
            b: Some(b),
            ..
        } => format!(
            "{}: {} vs {}",
            repograph::sanitize(field),
            json_scalar_text(a),
            json_scalar_text(b)
        ),
        Evidence::Pair {
            field,
            similarity: Some(sim),
            ..
        } => format!("{}: similarity {sim:.2}", repograph::sanitize(field)),
        Evidence::Pair { field, .. } => repograph::sanitize(field),
        Evidence::Parts { parts } => parts
            .iter()
            .map(evidence_summary)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// A JSON scalar as the digest prints it: a string without its quotes,
/// anything else as-is. Property values are graph content, so every string
/// goes through [`repograph::sanitize`].
fn json_scalar_text(v: &Js) -> String {
    match v {
        Js::String(s) => repograph::sanitize(s),
        other => other.to_string(),
    }
}

/// One header, then one line per rule-derived edge, capped like every other
/// task digest.
///
/// Rule names, edge types and predicate fields are all graph content — a rule
/// is named by whoever created it — so each goes through
/// [`repograph::sanitize`] before it reaches a line-structured digest.
fn render_explanations(a: &str, b: &str, found: &[ExplainedEdge]) -> String {
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
    for ExplainedEdge { edge: e, evidence } in found {
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
        out.push_str(&format!(" — {}", predicate_summary(&e.predicate)));
        // The matched values, in brackets, after the threshold that admitted
        // them: "overlap on specialties >= 0.2 [specialties: hospitality,
        // residential]". This is the line that stops an assistant fetching
        // both nodes' raw lists and reading out everything either one holds.
        if let Some(ev) = evidence {
            out.push_str(&format!(" [{}]", evidence_summary(ev)));
        }
        out.push('\n');
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
    label: Option<&str>,
) -> Result<(usize, Vec<EdgeGroup>), GraphError> {
    let edges = {
        let g = db.read();
        g.node_edges(key)?
    };
    let mut wanted_label = LabelFilter::new(db, label);

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
        if !wanted_label.keeps(&other) {
            continue;
        }
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

/// The `limit` argument, defaulted and clamped.
///
/// A `limit` past `max` is clamped rather than refused — the schema already
/// names the ceiling, and a caller who asks for more means "all of it" — but
/// a zero is a reply with counts and no rows, which no caller means.
fn limit_arg(args: &Js, default: usize, max: usize) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Js::Null) => Ok(default),
        Some(v) => match v.as_u64() {
            Some(0) | None => Err("limit must be a positive integer".into()),
            Some(n) => Ok(usize::try_from(n).unwrap_or(max).min(max)),
        },
    }
}

/// The `limit` argument: how many edges of each type to list.
fn edge_limit_arg(args: &Js) -> Result<usize, String> {
    limit_arg(args, DEFAULT_EDGE_LIMIT, MAX_EDGE_LIMIT)
}

// ── keys-only partner views ─────────────────────────────────────────────────

/// Partners listed by a keys-only view when the caller names no `limit`.
///
/// Twenty times the grouped view's default: a key is a few bytes where an
/// attributed edge line is a sentence, and the question these views answer —
/// *which* partners — is not answered by a tenth of the set.
const DEFAULT_PARTNER_LIMIT: usize = 200;

/// The largest `limit` a keys-only view honours. A partner set this wide is
/// still only tens of kilobytes, and it is what the caller asked for by name.
const MAX_PARTNER_LIMIT: usize = 2000;

/// Columns a wrapped key list fills before it breaks to the next line.
const KEY_WRAP_COLUMNS: usize = 100;

/// Keeps only the partners carrying one label.
///
/// The label is resolved per partner key and memoised, so a node joined by
/// four rules is looked up once rather than four times, and a filter with no
/// label answers `true` without touching the store at all. The label is the
/// one the node carries **now**, including when the edges being filtered come
/// from a past commit: a node's label is fixed when it is inserted.
struct LabelFilter<'a> {
    db: &'a SharedDb,
    label: Option<String>,
    seen: BTreeMap<String, bool>,
}

impl<'a> LabelFilter<'a> {
    fn new(db: &'a SharedDb, label: Option<&str>) -> Self {
        LabelFilter {
            db,
            label: label.map(str::to_string),
            seen: BTreeMap::new(),
        }
    }

    fn keeps(&mut self, key: &str) -> bool {
        let Some(label) = &self.label else {
            return true;
        };
        if let Some(hit) = self.seen.get(key) {
            return *hit;
        }
        let ok = {
            let g = self.db.read();
            g.node_ref(key).is_some_and(|n| n.label() == label)
        };
        self.seen.insert(key.to_string(), ok);
        ok
    }

    /// Drop every row whose partner does not carry the label.
    fn retain(&mut self, rows: &mut Vec<PartnerEdge>) {
        if self.label.is_none() {
            return;
        }
        rows.retain(|r| self.keeps(&r.other));
    }
}

/// One incident edge reduced to what a keys-only view needs: who is at the
/// other end, by what type, and in which direction.
struct PartnerEdge {
    other: String,
    edge_type: String,
    outgoing: bool,
}

impl PartnerEdge {
    /// `key` must be the node's canonical key — see [`canonical_self`].
    fn of(src_key: &str, dst_key: &str, edge_type: &str, key: &str) -> Self {
        let outgoing = src_key == key;
        PartnerEdge {
            other: if outgoing {
                dst_key.to_string()
            } else {
                src_key.to_string()
            },
            edge_type: edge_type.to_string(),
            outgoing,
        }
    }

    fn kept(&self, dir: Dir) -> bool {
        match dir {
            Dir::Out => self.outgoing,
            Dir::In => !self.outgoing,
            Dir::Both => true,
        }
    }
}

/// The `direction` argument of a keys-only view: `out`, `in`, or `any`
/// (the default). `both` is accepted as a synonym of `any`, because that is
/// what `neighborhood` calls the same thing.
fn partner_dir_arg(args: &Js) -> Result<Dir, String> {
    match args.get("direction") {
        None | Some(Js::Null) => Ok(Dir::Both),
        Some(v) => match v.as_str() {
            Some(s) if s.eq_ignore_ascii_case("out") => Ok(Dir::Out),
            Some(s) if s.eq_ignore_ascii_case("in") => Ok(Dir::In),
            Some(s) if s.eq_ignore_ascii_case("any") || s.eq_ignore_ascii_case("both") => {
                Ok(Dir::Both)
            }
            Some(other) => Err(format!("unknown direction: {}", repograph::sanitize(other))),
            None => Err("direction must be a string".into()),
        },
    }
}

/// The `all_of` argument: the edge types a partner must carry *every* one of.
fn all_of_arg(args: &Js) -> Result<Vec<String>, String> {
    let types = str_list_arg(args, "all_of")?;
    if args.get("all_of").is_some_and(|v| !v.is_null()) && types.is_empty() {
        return Err("all_of must name at least one edge type".into());
    }
    Ok(types)
}

/// The partners joined to the node by **every** type in `all_of`, sorted.
///
/// The intersection is over partners, not edges: a partner carrying two of
/// three named types is not in the answer, however many edges of those two it
/// has. Direction filters the edges considered, so `direction: "out"` asks
/// which partners the node points at by all of the types.
fn partners_linked_by_all(rows: &[PartnerEdge], all_of: &[String], dir: Dir) -> Vec<String> {
    let mut by_partner: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in rows {
        if !r.kept(dir) {
            continue;
        }
        by_partner
            .entry(r.other.as_str())
            .or_default()
            .insert(r.edge_type.as_str());
    }
    by_partner
        .into_iter()
        .filter(|(_, types)| all_of.iter().all(|t| types.contains(t.as_str())))
        .map(|(k, _)| k.to_string())
        .collect()
}

/// The distinct partners joined by one edge type, sorted, and how many edges
/// of that type there are — the two are equal unless a pair is joined twice.
fn partners_of_type(rows: &[PartnerEdge], edge_type: &str, dir: Dir) -> (usize, Vec<String>) {
    let mut edges = 0usize;
    let mut partners: BTreeSet<&str> = BTreeSet::new();
    for r in rows {
        if r.edge_type != edge_type || !r.kept(dir) {
            continue;
        }
        edges += 1;
        partners.insert(r.other.as_str());
    }
    (
        edges,
        partners.into_iter().map(str::to_string).collect::<Vec<_>>(),
    )
}

/// Append `keys` as `a, b, c`, continuing `lead` and wrapping at
/// [`KEY_WRAP_COLUMNS`].
///
/// Every key goes through [`repograph::sanitize`] — a key is graph content,
/// and these lines are line-structured digests like any other.
fn push_key_list(out: &mut String, lead: &str, keys: &[String]) {
    let mut line = lead.to_string();
    let mut empty = line.is_empty();
    for (i, k) in keys.iter().enumerate() {
        let k = repograph::sanitize(k);
        let comma = usize::from(i + 1 < keys.len());
        if !empty && line.len() + 1 + k.len() + comma > KEY_WRAP_COLUMNS {
            out.push_str(&line);
            out.push('\n');
            line.clear();
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(&k);
        if comma == 1 {
            line.push(',');
        }
        empty = false;
    }
    if !line.is_empty() {
        out.push_str(&line);
        out.push('\n');
    }
}

/// `lead: k, k, k` plus the `… and N more` the `limit` cut off.
fn push_partner_block(out: &mut String, lead: &str, partners: &[String], limit: usize) {
    let listed = &partners[..partners.len().min(limit)];
    push_key_list(out, lead, listed);
    if partners.len() > listed.len() {
        out.push_str(&format!("… and {} more\n", partners.len() - listed.len()));
    }
}

/// The keys-only answer to "which partners are linked by all of these types".
///
/// Nothing here is capped by line count: `limit` is the cap, the trailing
/// `… and N more` says what it cut, and a capped digest that swallowed that
/// line would be the one shape a caller could not tell from a complete answer.
fn render_all_of(
    tool: &str,
    key: &str,
    at: Option<u64>,
    all_of: &[String],
    partners: &[String],
    limit: usize,
) -> String {
    let types = all_of
        .iter()
        .map(|t| repograph::sanitize(t))
        .collect::<Vec<_>>()
        .join(", ");
    let when = at.map_or_else(String::new, |a| format!(" as of commit {a}"));
    let mut out = format!(
        "mushroomdb {tool} — {}{when} — partners linked by all of {types}: {}\n",
        repograph::sanitize(key),
        partners.len()
    );
    if partners.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    push_partner_block(&mut out, "", partners, limit);
    out
}

/// The `json: true` shape of a keys-only partner answer.
fn partners_json(
    key: &str,
    at: Option<u64>,
    all_of: &[String],
    label: Option<&str>,
    partners: &[String],
    limit: usize,
) -> Js {
    let mut doc = json!({
        "key": key,
        "all_of": all_of,
        "partners": &partners[..partners.len().min(limit)],
        "total": partners.len(),
    });
    if let Some(a) = at {
        doc["at"] = json!(a);
    }
    if let Some(l) = label {
        doc["label"] = json!(l);
    }
    doc
}

/// The keys-only answer to "which partners does one edge type join".
///
/// The rule is printed once, in the type's header: every edge of a type comes
/// from the rule that declares that type, so repeating it per line said the
/// same words as many times as there were partners.
fn render_type_partners(
    header: String,
    edge_type: &str,
    edges: usize,
    rule: Option<&str>,
    partners: &[String],
    limit: usize,
) -> String {
    let mut out = header;
    if partners.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    let rule = rule.map_or_else(String::new, |r| {
        format!(", rule {}", repograph::sanitize(r))
    });
    let lead = format!("{} ({edges}{rule}):", repograph::sanitize(edge_type));
    push_partner_block(&mut out, &lead, partners, limit);
    out
}

/// The `json: true` shape of a one-type keys-only answer.
fn type_partners_json(
    key: &str,
    at: Option<u64>,
    edge_type: &str,
    rule: Option<&str>,
    edges: usize,
    partners: &[String],
    limit: usize,
) -> Js {
    let mut doc = json!({
        "key": key,
        "edge_type": edge_type,
        "rule": rule,
        "edges": edges,
        "partners": &partners[..partners.len().min(limit)],
        "total": partners.len(),
    });
    if let Some(a) = at {
        doc["at"] = json!(a);
    }
    doc
}

/// Record the `label` a reply was narrowed by, when it was narrowed at all.
fn with_label(mut doc: Js, label: Option<&str>) -> Js {
    if let Some(l) = label {
        doc["label"] = json!(l);
    }
    doc
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
    let all_of = match all_of_arg(args) {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let dir = match partner_dir_arg(args) {
        Ok(d) => d,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let label = match opt_str_arg(args, "label") {
        Ok(l) => l,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    if !all_of.is_empty() || edge_type.is_some() {
        let limit = match limit_arg(args, DEFAULT_PARTNER_LIMIT, MAX_PARTNER_LIMIT) {
            Ok(n) => n,
            Err(e) => return CallOutcome::ToolErr(e),
        };
        let edges = {
            let g = db.read();
            match g.node_edges(key) {
                Ok(v) => v,
                Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
            }
        };
        let mut rows: Vec<PartnerEdge> = edges
            .iter()
            .map(|e| PartnerEdge::of(&e.src_key, &e.dst_key, &e.edge_type, key))
            .collect();
        LabelFilter::new(db, label).retain(&mut rows);
        if !all_of.is_empty() {
            let partners = partners_linked_by_all(&rows, &all_of, dir);
            let report = partners_json(key, None, &all_of, label, &partners, limit);
            return ok(json_out, &report, |_| {
                render_all_of("edges", key, None, &all_of, &partners, limit)
            });
        }
        let edge_type = edge_type.unwrap_or_default();
        let (count, partners) = partners_of_type(&rows, edge_type, dir);
        let rule = live_rule_for_type(db, key, edge_type, partners.first());
        let report = with_label(
            type_partners_json(
                key,
                None,
                edge_type,
                rule.as_deref(),
                count,
                &partners,
                limit,
            ),
            label,
        );
        return ok(json_out, &report, |_| {
            let header = format!(
                "mushroomdb edges — {}: {count} edge(s) over {} type(s)\n",
                repograph::sanitize(key),
                usize::from(count > 0)
            );
            render_type_partners(header, edge_type, count, rule.as_deref(), &partners, limit)
        });
    }

    let limit = match edge_limit_arg(args) {
        Ok(n) => n,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    edge_reply(db, key, None, dir, limit, label, json_out)
}

/// The rule behind one edge type on a live node, from a single `explain` call
/// on one partner — every edge of a type is written by the one rule that
/// declares it, so one pair answers for the whole type. `None` for a manual
/// edge, which no rule derived and which has nothing to name.
fn live_rule_for_type(
    db: &SharedDb,
    key: &str,
    edge_type: &str,
    partner: Option<&String>,
) -> Option<String> {
    let partner = partner?;
    let g = db.read();
    g.explain(key, partner)
        .unwrap_or_default()
        .into_iter()
        .find(|e| e.edge_type == edge_type)
        .map(|e| e.rule)
}

/// Group, render and answer — the tail both `node_edges` and a depth-1
/// `neighborhood` share.
fn edge_reply(
    db: &SharedDb,
    key: &str,
    types: Option<&[String]>,
    dir: Dir,
    limit: usize,
    label: Option<&str>,
    json_out: bool,
) -> CallOutcome {
    match node_edge_groups(db, key, types, dir, limit, label) {
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
        return edge_reply(db, key, filter, dir, limit, None, json_out);
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

// ── edges_at / what_if shared rendering ─────────────────────────────────────

/// One edge in an `edges_at` or `what_if` reply, as a document: the same
/// shape whichever tool built it, so a caller reading `json: true` sees one
/// edge schema across both.
fn edge_at_json(e: &core_api::EdgeAt) -> Js {
    json!({
        "edge_type": e.edge_type,
        "src": e.src_key,
        "dst": e.dst_key,
        "derived": e.derived,
        "rule": e.rule,
    })
}

// ── edges_at ─────────────────────────────────────────────────────────────────

/// One edge in an `edges_at` text reply, direction already resolved relative
/// to the node the call was about.
struct EdgeAtLine {
    other: String,
    outgoing: bool,
    rule: Option<String>,
}

/// Every edge of one type at the queried commit, and the slice of them listed.
struct EdgeAtGroup {
    edge_type: String,
    count: usize,
    listed: Vec<EdgeAtLine>,
}

/// Group `edges` — every one of them already incident to `key`, sorted by
/// `(edge_type, src_key, dst_key)` by the engine — by edge type, keeping at
/// most `limit` per type for the digest.
fn edges_at_groups(
    edges: Vec<core_api::EdgeAt>,
    key: &str,
    limit: usize,
) -> (usize, Vec<EdgeAtGroup>) {
    let mut by_type: BTreeMap<String, Vec<core_api::EdgeAt>> = BTreeMap::new();
    let mut total = 0usize;
    for e in edges {
        total += 1;
        by_type.entry(e.edge_type.clone()).or_default().push(e);
    }
    let mut groups = Vec::with_capacity(by_type.len());
    for (edge_type, rows) in by_type {
        let count = rows.len();
        let listed = rows
            .into_iter()
            .take(limit)
            .map(|e| {
                let outgoing = e.src_key == key;
                let other = if outgoing { e.dst_key } else { e.src_key };
                EdgeAtLine {
                    other,
                    outgoing,
                    rule: e.rule,
                }
            })
            .collect();
        groups.push(EdgeAtGroup {
            edge_type,
            count,
            listed,
        });
    }
    (total, groups)
}

fn render_edges_at(key: &str, at: u64, total: usize, groups: &[EdgeAtGroup]) -> String {
    let mut out = format!(
        "mushroomdb edges_at — {} as of commit {at}: {total} edge(s)\n",
        repograph::sanitize(key)
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
            out.push('\n');
        }
        if g.count > g.listed.len() {
            out.push_str(&format!("  … and {} more\n", g.count - g.listed.len()));
        }
    }
    repograph::cap_lines(&out, MAX_EDGE_LINES)
}

/// The node's canonical current key, recovered from `edges` — every one of
/// them already reported by [`GraphDb::edges_at`] under the name the node
/// carries today, even when the caller queried by an old alias of a renamed
/// node (see `edges_at_reports_a_renamed_nodes_edges_under_the_current_key`
/// in `crates/core-api/tests/edges_at.rs`). A stale `key` therefore never
/// appears as one of its own edges' endpoints: comparing it directly against
/// `src_key`/`dst_key` (as this tool used to) inverts every arrow and swaps
/// the node for its partner.
///
/// There is no public accessor for the canonicalization `edges_at` does
/// internally, so this recovers it from the data instead: the one endpoint
/// every returned edge has in common is the node itself, found by
/// intersecting each edge's `{src_key, dst_key}` (a self-loop contributes
/// just the one key). A single edge to a single partner has nothing to
/// triangulate from — the two endpoints are symmetric from the outside — so
/// `key` is kept as-is in that case, and whenever there are no edges at all
/// (an unrecognized key and a recognized one with nothing at `at` look
/// identical here, matching `edges_at`'s own "unknown key is not an error"
/// contract).
fn canonical_self(edges: &[core_api::EdgeAt], key: &str) -> String {
    let mut candidates: Option<BTreeSet<&str>> = None;
    for e in edges {
        let this_edge: BTreeSet<&str> = if e.src_key == e.dst_key {
            std::iter::once(e.src_key.as_str()).collect()
        } else {
            [e.src_key.as_str(), e.dst_key.as_str()]
                .into_iter()
                .collect()
        };
        candidates = Some(match candidates {
            None => this_edge,
            Some(prev) => prev.intersection(&this_edge).copied().collect(),
        });
    }
    match candidates {
        Some(c) if c.len() == 1 => c.into_iter().next().unwrap().to_string(),
        _ => key.to_string(),
    }
}

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
    let edge_type = match opt_str_arg(args, "edge_type") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let all_of = match all_of_arg(args) {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let dir = match partner_dir_arg(args) {
        Ok(d) => d,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let label = match opt_str_arg(args, "label") {
        Ok(l) => l,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let keys_only = !all_of.is_empty() || edge_type.is_some();
    let limit = match if keys_only {
        limit_arg(args, DEFAULT_PARTNER_LIMIT, MAX_PARTNER_LIMIT)
    } else {
        edge_limit_arg(args)
    } {
        Ok(n) => n,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    let edges = {
        let g = db.read();
        match g.edges_at(key, at) {
            Ok(v) => v,
            Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
        }
    };
    let self_key = canonical_self(&edges, key);

    if keys_only {
        let mut rows: Vec<PartnerEdge> = edges
            .iter()
            .map(|e| PartnerEdge::of(&e.src_key, &e.dst_key, &e.edge_type, &self_key))
            .collect();
        LabelFilter::new(db, label).retain(&mut rows);
        if !all_of.is_empty() {
            let partners = partners_linked_by_all(&rows, &all_of, dir);
            let report = partners_json(&self_key, Some(at), &all_of, label, &partners, limit);
            return ok(json_out, &report, |_| {
                render_all_of("edges_at", &self_key, Some(at), &all_of, &partners, limit)
            });
        }
        let edge_type = edge_type.unwrap_or_default();
        let (count, partners) = partners_of_type(&rows, edge_type, dir);
        // The rule as it stood at `at`, off the edges themselves — every edge
        // of a type carries the same one, so the first is the type's.
        let rule = edges
            .iter()
            .find(|e| e.edge_type == edge_type && e.rule.is_some())
            .and_then(|e| e.rule.clone());
        let report = with_label(
            type_partners_json(
                &self_key,
                Some(at),
                edge_type,
                rule.as_deref(),
                count,
                &partners,
                limit,
            ),
            label,
        );
        return ok(json_out, &report, |_| {
            let header = format!(
                "mushroomdb edges_at — {} as of commit {at}: {count} edge(s)\n",
                repograph::sanitize(&self_key)
            );
            render_type_partners(header, edge_type, count, rule.as_deref(), &partners, limit)
        });
    }

    // The grouped view, narrowed to the partners carrying `label` if one was
    // named — counts included, so the header says what the filter left.
    let mut filter = LabelFilter::new(db, label);
    let edges: Vec<core_api::EdgeAt> = edges
        .into_iter()
        .filter(|e| {
            let other = if e.src_key == self_key {
                &e.dst_key
            } else {
                &e.src_key
            };
            filter.keeps(other)
        })
        .collect();

    // The report lists what the text lists: at most `limit` per edge type,
    // the engine's own order. Without this an edges_at report of a hub node
    // was a hundred kilobytes of JSON no caller had asked for.
    let mut per_type: BTreeMap<&str, usize> = BTreeMap::new();
    let listed: Vec<Js> = edges
        .iter()
        .filter(|e| {
            let n = per_type.entry(e.edge_type.as_str()).or_default();
            *n += 1;
            *n <= limit
        })
        .map(edge_at_json)
        .collect();
    let report = with_label(
        json!({
            "key": self_key,
            "at": at,
            "edges": listed,
            "listed": listed.len(),
            "total": edges.len(),
        }),
        label,
    );
    let (total, groups) = edges_at_groups(edges, &self_key, limit);
    ok(json_out, &report, |_| {
        render_edges_at(&self_key, at, total, &groups)
    })
}

// ── what_if ──────────────────────────────────────────────────────────────────

/// One edge in a `what_if` text reply. `incident` is `false` for a derived
/// edge the change churns elsewhere in the graph — [`GraphDb::what_if_set_prop`]
/// can report those alongside the ones touching the changed node, and they
/// still need a rule and a partner even though neither endpoint is `key`.
struct WhatIfLine {
    src: String,
    dst: String,
    rule: Option<String>,
    incident: bool,
    outgoing: bool,
}

struct WhatIfGroup {
    edge_type: String,
    count: usize,
    lines: Vec<WhatIfLine>,
}

/// Group `edges` by type, incident edges first within each group — the ones
/// that touch `key` are the answer to the question asked; edges churned
/// elsewhere are secondary and sort after them, in the engine's own order.
fn what_if_groups(edges: &[core_api::EdgeAt], key: &str) -> Vec<WhatIfGroup> {
    let mut by_type: BTreeMap<String, Vec<WhatIfLine>> = BTreeMap::new();
    for e in edges {
        let outgoing = e.src_key == key;
        let incident = outgoing || e.dst_key == key;
        by_type
            .entry(e.edge_type.clone())
            .or_default()
            .push(WhatIfLine {
                src: e.src_key.clone(),
                dst: e.dst_key.clone(),
                rule: e.rule.clone(),
                incident,
                outgoing,
            });
    }
    let mut groups = Vec::with_capacity(by_type.len());
    for (edge_type, mut lines) in by_type {
        // Stable sort: incident edges keep their engine order ahead of the
        // non-incident ones, which keep theirs.
        lines.sort_by_key(|l| !l.incident);
        let count = lines.len();
        groups.push(WhatIfGroup {
            edge_type,
            count,
            lines,
        });
    }
    groups
}

fn render_what_if_groups(out: &mut String, groups: &[WhatIfGroup], limit: usize) {
    if groups.is_empty() {
        out.push_str("  none\n");
        return;
    }
    for g in groups {
        out.push_str(&format!(
            "  {} ({})\n",
            repograph::sanitize(&g.edge_type),
            g.count
        ));
        for l in g.lines.iter().take(limit) {
            if l.incident {
                let arrow = if l.outgoing { "→" } else { "←" };
                let other = if l.outgoing { &l.dst } else { &l.src };
                out.push_str(&format!("    {arrow} {}", repograph::sanitize(other)));
            } else {
                out.push_str(&format!(
                    "    {} → {}",
                    repograph::sanitize(&l.src),
                    repograph::sanitize(&l.dst)
                ));
            }
            if let Some(rule) = &l.rule {
                out.push_str(&format!("  rule {}", repograph::sanitize(rule)));
            }
            out.push('\n');
        }
        if g.count > limit {
            out.push_str(&format!("    … and {} more\n", g.count - limit));
        }
    }
}

fn render_what_if(
    key: &str,
    field: &str,
    value: &Js,
    lost: &[WhatIfGroup],
    gained: &[WhatIfGroup],
    limit: usize,
) -> String {
    let edges = |gs: &[WhatIfGroup]| gs.iter().map(|g| g.count).sum::<usize>();
    let mut out = what_if_header(key, field, value, edges(lost), edges(gained));
    out.push_str("lost\n");
    render_what_if_groups(&mut out, lost, limit);
    out.push_str("gained\n");
    render_what_if_groups(&mut out, gained, limit);
    repograph::cap_lines(&out, MAX_EDGE_LINES)
}

fn what_if_header(
    key: &str,
    field: &str,
    value: &Js,
    lost_total: usize,
    gained_total: usize,
) -> String {
    format!(
        "mushroomdb what_if — {}.{} = {}: would lose {lost_total}, would gain {gained_total}\n",
        repograph::sanitize(key),
        repograph::sanitize(field),
        repograph::sanitize(&value.to_string()),
    )
}

/// The distinct nodes one side of a `what_if` touches, sorted.
///
/// An edge incident on the changed node is named by its partner — the answer
/// to "which partners does this cost me". An edge the change churns elsewhere
/// in the graph has no partner to name, so it is written out as the pair it
/// is, rather than being dropped from a reply that counts it.
fn what_if_partners(edges: &[core_api::EdgeAt], key: &str) -> Vec<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for e in edges {
        if e.src_key == key {
            set.insert(e.dst_key.clone());
        } else if e.dst_key == key {
            set.insert(e.src_key.clone());
        } else {
            set.insert(format!("{} → {}", e.src_key, e.dst_key));
        }
    }
    set.into_iter().collect()
}

/// The keys-only `what_if` reply: one edge type, so one rule, so two lists of
/// keys under `lost` and `gained`.
fn render_what_if_keys_only(
    key: &str,
    field: &str,
    value: &Js,
    edge_type: &str,
    lost: &[core_api::EdgeAt],
    gained: &[core_api::EdgeAt],
    limit: usize,
) -> String {
    let mut out = what_if_header(key, field, value, lost.len(), gained.len());
    for (heading, side) in [("lost\n", lost), ("gained\n", gained)] {
        out.push_str(heading);
        let partners = what_if_partners(side, key);
        if partners.is_empty() {
            out.push_str("  none\n");
            continue;
        }
        let rule = side
            .iter()
            .find_map(|e| e.rule.as_deref())
            .map_or_else(String::new, |r| {
                format!(", rule {}", repograph::sanitize(r))
            });
        let lead = format!("{} ({}{rule}):", repograph::sanitize(edge_type), side.len());
        push_partner_block(&mut out, &lead, &partners, limit);
    }
    out
}

/// What changes if `key.field` became `value` — computed directly by the
/// engine ([`GraphDb::what_if_set_prop`]) without writing anything: no copy
/// of the store is made, so nothing here needs to know where it lives on
/// disk.
fn tool_what_if(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let key = match str_arg(args, "key") {
        Ok(k) => k,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let field = match str_arg(args, "field") {
        Ok(f) => f,
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

    let edge_type = match opt_str_arg(args, "edge_type") {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let label = match opt_str_arg(args, "label") {
        Ok(l) => l,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let limit = match limit_arg(args, DEFAULT_EDGE_LIMIT, MAX_PARTNER_LIMIT) {
        Ok(n) => n,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    let wi = {
        let g = db.read();
        g.what_if_set_prop(key, field, value)
    };
    let wi = match wi {
        Ok(w) => w,
        Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
    };

    // `edge_type` and `label` narrow what is counted as well as what is
    // listed: the reply is then about that type, or those partners, totals
    // included. An edge the change churns elsewhere in the graph has no
    // partner to carry a label, so a labelled call leaves it out.
    let mut filter = LabelFilter::new(db, label);
    let mut pick = |edges: &'_ [core_api::EdgeAt]| -> Vec<core_api::EdgeAt> {
        edges
            .iter()
            .filter(|e| edge_type.is_none_or(|t| e.edge_type == t))
            .filter(|e| match (e.src_key == key, e.dst_key == key) {
                (true, _) => filter.keeps(&e.dst_key),
                (_, true) => filter.keeps(&e.src_key),
                _ => label.is_none(),
            })
            .cloned()
            .collect()
    };
    let lost = pick(&wi.lost);
    let gained = pick(&wi.gained);

    let doc = |edges: &[core_api::EdgeAt]| -> Vec<Js> {
        edges
            .iter()
            .take(limit)
            .map(edge_at_json)
            .collect::<Vec<_>>()
    };
    let mut report = with_label(
        json!({
            "key": key,
            "field": field,
            "value": raw,
            "lost": doc(&lost),
            "lost_total": lost.len(),
            "gained": doc(&gained),
            "gained_total": gained.len(),
        }),
        label,
    );
    if let Some(t) = edge_type {
        report["edge_type"] = json!(t);
    }

    if let Some(t) = edge_type {
        return ok(json_out, &report, |_| {
            render_what_if_keys_only(key, field, raw, t, &lost, &gained, limit)
        });
    }
    let lost_groups = what_if_groups(&lost, key);
    let gained_groups = what_if_groups(&gained, key);
    ok(json_out, &report, |_| {
        render_what_if(key, field, raw, &lost_groups, &gained_groups, limit)
    })
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
            "description": "Why are A and B related — every relationship between the two keys and its evidence: the rule that derived it, its edge type, the match score, the predicate it matched on, and the values the two actually share (which specialties overlapped, which field was equal, how far apart they are). Answer from those shared values; the nodes' full property lists name everything either one holds, not what they have in common. Both keys must already exist.",
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
            "description": "What is K related to — every relationship of one node, grouped by edge type with a count, each listed edge carrying its direction, the rule that derived it, its score and the predicate it matched on. Answers 'why is this here' in the same call that lists it, so no follow-up explain is needed. Which partners are linked by all of these types? pass all_of and the reply is just their keys; pass one edge_type for that type's partner keys with the rule named once. label narrows partners.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key." },
                    "edge_type": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only this type: the reply is that type's partner keys, compactly. Omit for the grouped listing over every type."
                    },
                    "all_of": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Only the partners linked by EVERY one of these types — the intersection, as keys."
                    },
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only partners carrying this node label, counts included."
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["out", "in", "any"],
                        "description": "Which edges count (default any)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "Edges listed per edge type (default 10), or partner keys listed under edge_type/all_of (default 200, max 2000). The rest are counted as '… and N more'."
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
            "description": "What did K's relationships look like at commit C — the edges that were live at one point in the store's history, with the rule that had derived each. `at` is a 0-based WAL commit index; use node_history or edge_history first to find the commit you want, then read this instead of replaying either by hand. Which partners were linked by all of these types on that day? pass all_of and the reply is just their keys; pass one edge_type for that type's partner keys with the rule named once. label narrows partners.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key." },
                    "at": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "0-based WAL commit index to read the edges at."
                    },
                    "edge_type": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only this type: the reply is that type's partner keys, compactly. Omit for the grouped listing over every type."
                    },
                    "all_of": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Only the partners linked by EVERY one of these types at that commit — the intersection, as keys."
                    },
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only partners carrying this node label, counts included."
                    },
                    "direction": {
                        "type": "string",
                        "enum": ["out", "in", "any"],
                        "description": "Which edges count (default any)."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "Edges listed per edge type (default 10, max 100), or partner keys listed under edge_type/all_of (default 200, max 2000). The rest are counted as '… and N more'."
                    }
                },
                "required": ["key", "at"]
            }
        }),
        json!({
            "name": "what_if",
            "description": "What changes if K's FIELD became VALUE — the relationships lost and gained, with the rule behind each. Does not change the store: nothing is written, nothing on disk is copied, and the live graph answers the same way before and after the call. Which partners of one type would it cost? pass edge_type and the lost and gained lists are just their keys, with the rule named once. label narrows partners.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key to change." },
                    "field": { "type": "string", "minLength": 1, "description": "Property name to set." },
                    "value": {
                        "description": "The value it would take: a string, number, boolean, or a list or map of those. Not null."
                    },
                    "edge_type": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only this type, counts included: the reply is the partner keys lost and gained, compactly."
                    },
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only partners carrying this node label, counts included."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "Edges (or partner keys) listed per side (default 10). The rest are counted as '… and N more'."
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
        let one = |rule: &str, via: Option<&str>| ExplainedEdge {
            edge: Explanation {
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
            },
            // A via-hop rule matched between the via node and the
            // destination, so there is no pair here to show evidence from.
            evidence: None,
        };

        let text = render_explanations("a", "b", &[one("close", Some("WORKS_AT"))]);
        assert_eq!(
            text,
            "mushroomdb explain — a ↔ b: 1 relationship(s)\n  SIMILAR via rule close (score 0.96) \
             via WORKS_AT — vector_similar on emb >= 0.85\n"
        );

        let many: Vec<ExplainedEdge> = (0..40).map(|i| one(&format!("r{i}"), None)).collect();
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

    fn edge_at(edge_type: &str, src: &str, dst: &str) -> core_api::EdgeAt {
        core_api::EdgeAt {
            edge_type: edge_type.into(),
            src_key: src.into(),
            dst_key: dst.into(),
            derived: false,
            rule: None,
        }
    }

    /// Binding: `canonical_self` recovers the node's identity by intersecting
    /// the endpoints of every edge it appears in — the fix for `edges_at`
    /// comparing a stale alias against the engine's already-canonicalized
    /// endpoints (which would otherwise invert every arrow and report the
    /// node as its own partner).
    #[test]
    fn canonical_self_intersects_endpoints_across_edges() {
        // Two edges to two different partners narrow to exactly one shared
        // endpoint: the node itself, even though `key` ("old") never appears
        // in either edge — this is what querying `edges_at` by a stale alias
        // of a renamed node looks like once the engine has canonicalized the
        // output to the current key ("new").
        let edges = vec![edge_at("Knows", "new", "p1"), edge_at("Knows", "p2", "new")];
        assert_eq!(canonical_self(&edges, "old"), "new");

        // A single edge to a single partner is symmetric — there is nothing
        // to triangulate from — so the raw key is kept rather than guessed.
        let one_edge = vec![edge_at("Knows", "new", "p1")];
        assert_eq!(canonical_self(&one_edge, "old"), "old");

        // The common, non-renamed case: `key` already appears in the edges,
        // so it is returned unchanged even when candidates cannot narrow to
        // one (self-loop aside, this is the fallback path's every-day case).
        let unrenamed = vec![edge_at("Knows", "a", "b")];
        assert_eq!(canonical_self(&unrenamed, "a"), "a");

        // No edges at all: nothing to recover from, `key` is kept as-is.
        assert_eq!(canonical_self(&[], "whatever"), "whatever");

        // A self-loop contributes just the one key to the candidate set.
        let self_loop = vec![
            edge_at("Knows", "new", "new"),
            edge_at("Likes", "new", "p1"),
        ];
        assert_eq!(canonical_self(&self_loop, "old"), "new");
    }
}
