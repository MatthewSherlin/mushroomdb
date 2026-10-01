//! The MCP tools that answer a question in prose rather than in JSON.
//!
//! They sit in front of the fourteen graph tools in `mcp::tools_list`.
//! `explain_association` answers why two entities are associated, with the
//! rule that derived each edge; `recall` answers what is already known about
//! a topic, and `remember` writes a note for next time.
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
//! All but one are pure reads of the graph. `remember` writes one `Note`.
//!
//! # Untrusted content
//!
//! Everything these tools render came out of the graph, and a graph holds
//! whatever its writers put there: an `ingest-git` store carries author names,
//! paths, commit subjects and doc comments, and a memory store carries notes.
//! [`ok`] therefore stamps every reply with
//! [`core_api::digest::UNTRUSTED_FRAMING`], the same marker the prompt hook
//! and the session brief put on theirs, so an assistant is told to read the
//! lines under it as data before it reads any of them. The renderers already
//! sanitize each line; the framing is what says whose words they are.

use crate::mcp::{graph_err_msg, CallOutcome};
use core_api::digest::{self, MAX_OUTPUT_BYTES, UNTRUSTED_FRAMING};
use core_api::explain_digest::{explain_with_evidence, predicate_summary, render_explain};
use core_api::memory::remember::{remember, EntityIn, FactIn, RememberInput, NOTE_KINDS};
use core_api::{json_to_value, Dir, Explanation, GraphError, SharedDb, Value};
use serde_json::{json, Value as Js};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The names this module answers to. Listed once, so the `json`
/// argument below is read for exactly the tools that declare it.
///
/// `explain_association` comes first: what links these two, with the evidence
/// — the rules that derived the edge. The four entity tools follow it, because
/// they are the same question widened: every relationship of one node rather
/// than of one pair, that listing at a past commit, and that listing under a
/// change that has not been made.
pub(crate) const TASK_TOOLS: [&str; 11] = [
    "explain_association",
    "node_edges",
    "neighborhood",
    "edges_at",
    "what_if",
    "recall",
    "remember",
    "schema",
    "analyze",
    "suggest_rules",
    "forget",
];

/// Route a task tool. `None` when `name` is not one of [`TASK_TOOLS`].
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
    // type-checked once here rather than once per tool — and before any work, so
    // a caller that mistyped it is told so rather than served a digest it did
    // not ask for.
    let json_out = match bool_arg(args, "json") {
        Ok(b) => b,
        Err(e) => return Some(CallOutcome::ToolErr(e)),
    };
    Some(match name {
        "explain_association" => tool_explain_association(db, args, json_out),
        "node_edges" => tool_node_edges(db, args, json_out),
        "neighborhood" => tool_neighborhood(db, args, json_out),
        "edges_at" => tool_edges_at(db, args, json_out),
        "what_if" => tool_what_if(db, args, json_out),
        "recall" => tool_recall(db, db_dir, args, json_out),
        "remember" => tool_remember(db, args, json_out),
        "schema" => tool_schema(db, json_out),
        "analyze" => tool_analyze(db, args, json_out),
        "suggest_rules" => tool_suggest_rules(db, json_out),
        "forget" => tool_forget(db, args, json_out),
        _ => unreachable!("TASK_TOOLS and this match list the same names"),
    })
}

/// A successful task reply.
///
/// With `json_out` clear — the default — it is the rendered digest under the
/// untrusted-data framing line, and nothing else: no `structuredContent`, no
/// second copy of the same text. No renderer here frames its own output —
/// `recall_digest` included — so this is the one place the line is stamped.
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
    CallOutcome::TaskOk {
        text: format!("{UNTRUSTED_FRAMING}{}", render(report)),
    }
}

/// Replace the control characters in every string of `value` with spaces.
///
/// Graph content reaches a JSON reply in the **values**: paths, author names,
/// commit subjects, note text. The keys are the report's own field names,
/// fixed in the Rust types the reports serialise from, so they carry nothing
/// an outsider wrote and are left alone — rewriting a key could silently merge
/// two of them.
///
/// Newline and tab survive; every other control character does not. That is
/// the one place this differs from [`digest::sanitize`], and the reason is
/// what the two channels are. A digest is line-structured, so a newline inside
/// a value could forge a heading or an extra hit and has to go. A JSON value is
/// delimited by the grammar, so a newline inside one cannot escape it — and
/// some of these values *are* multi-line documents: `recall`'s report carries
/// the whole rendered digest. Flattening that would corrupt the report to
/// defend against nothing. What is still removed is everything that acts on a
/// reader whatever contains it: escape sequences, carriage returns that
/// overwrite a line, backspace, `DEL`.
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
    // A key the graph does not hold is an error here, not an empty answer:
    // `explain` resolves both keys to dense ids before it
    // looks at a single edge, and that is the engine's answer to give.
    let report = {
        let g = db.read();
        match explain_with_evidence(&g, &a, &b) {
            Ok(v) => v,
            Err(e) => return CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
        }
    };
    ok(json_out, &report, |found| render_explain(&a, &b, found))
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

/// Longest edge digest, in lines. Wider than [`digest::MAX_TOOL_LINES`]
/// because this listing is the reply an assistant reads instead of calling
/// `query` twenty times, and a default `limit` over four edge types already
/// runs past twenty-five lines. The header counts every edge whatever is
/// printed, so a capped digest still says how much it is not showing.
const MAX_EDGE_LINES: usize = digest::MAX_MAP_LINES;

/// Cap a grouped digest at [`MAX_EDGE_LINES`], saying so when it cuts.
///
/// A listing over many edge types runs past the line budget even under a
/// small `limit`, and a budget that cut silently looked exactly like a
/// complete reply — the caller could not tell. The compact forms are the way
/// past it, so the marker names them.
fn cap_grouped(out: &str) -> String {
    if out.lines().count() <= MAX_EDGE_LINES {
        return out.to_string();
    }
    let mut capped = digest::cap_lines(out, MAX_EDGE_LINES);
    capped.push_str(&format!(
        "… listing capped at {MAX_EDGE_LINES} lines; pass edge_type or all_of for the whole set\n"
    ));
    capped
}

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
/// content, so every one of them goes through [`digest::sanitize`] before
/// it reaches a line-structured digest.
fn render_edge_groups(key: &str, total: usize, groups: &[EdgeGroup]) -> String {
    let mut out = format!(
        "mushroomdb edges — {}: {total} edge(s) over {} type(s)\n",
        digest::sanitize(key),
        groups.len()
    );
    if groups.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for g in groups {
        out.push_str(&format!(
            "{} ({})\n",
            digest::sanitize(&g.edge_type),
            g.count
        ));
        for e in &g.listed {
            let arrow = if e.outgoing { "→" } else { "←" };
            out.push_str(&format!("  {arrow} {}", digest::sanitize(&e.other)));
            if let Some(rule) = &e.rule {
                out.push_str(&format!("  rule {}", digest::sanitize(rule)));
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
    cap_grouped(&out)
}

/// The same grouping as a document, for `json: true`.
///
/// `listed` is the top-level count of edges actually in `types[].edges`, the
/// sum of the per-type `listed`. It is what the tool description and the docs
/// promise — "the report carries `listed` and `total`, so a reply that was cut
/// still says how much there was" — and without it a caller had to add the
/// per-type counts up itself to learn whether the reply was whole.
///
/// `label` is echoed when the reply was narrowed by one, the way every other
/// shape of this reply echoes it: a document that does not say what it was
/// filtered by reads as the unfiltered answer.
fn edge_groups_json(key: &str, total: usize, groups: &[EdgeGroup], label: Option<&str>) -> Js {
    let doc = json!({
        "key": key,
        "total": total,
        "listed": groups.iter().map(|g| g.listed.len()).sum::<usize>(),
        "types": groups.iter().map(|g| json!({
            "edge_type": g.edge_type,
            "count": g.count,
            "listed": g.listed.len(),
            "edges": g.listed.iter().map(edge_line_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    with_label(doc, label)
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

/// Refusing the one pair of arguments that cannot both be honoured: the
/// answer is either an intersection over several types or a listing of one,
/// and silently letting either win would be a reply about a question the
/// caller did not ask.
const ONE_OF_ALL_OF_OR_EDGE_TYPE: &str = "pass one of all_of or edge_type, not both";

/// Columns a wrapped key list fills before it breaks to the next line.
const KEY_WRAP_COLUMNS: usize = 100;

/// Edge type names an "no such edge type" error lists before it counts the
/// rest off. Twenty is enough to recognise the one that was meant on any
/// schema a person designed, and short enough that the error is still an
/// error rather than a schema dump.
const MAX_KNOWN_EDGE_TYPES: usize = 20;

/// The error a keys-only view answers when the type it was asked about is not
/// in the store at all.
///
/// "0 partners" is the *right* answer for a type that exists and this node has
/// none of, and the wrong one for a typo — and the caller cannot tell the two
/// apart, so a misspelling reads as a fact about the graph. The census is the
/// store's own list of edge types, so the reply both names what went wrong and
/// carries what to ask instead.
///
/// `known` is only walked when the answer came back empty: a type that matched
/// something is a type that exists, and the census costs a pass over the
/// topology.
fn unknown_edge_type(db: &SharedDb, named: &[String]) -> Option<String> {
    let known: Vec<String> = {
        let g = db.read();
        g.edge_type_census()
            .into_iter()
            .map(|c| c.edge_type)
            .collect()
    };
    let missing: Vec<&String> = named.iter().filter(|t| !known.contains(t)).collect();
    if missing.is_empty() {
        return None;
    }
    let listed: Vec<String> = known
        .iter()
        .take(MAX_KNOWN_EDGE_TYPES)
        .map(|t| digest::sanitize(t))
        .collect();
    let rest = known.len().saturating_sub(listed.len());
    let more = if rest > 0 {
        format!(", … and {rest} more")
    } else {
        String::new()
    };
    let names = missing
        .iter()
        .map(|t| digest::sanitize(t))
        .collect::<Vec<_>>()
        .join(", ");
    Some(if known.is_empty() {
        format!("no edge type named {names}; this store has no edges yet")
    } else {
        format!(
            "no edge type named {names}; this store has: {}{more}",
            listed.join(", ")
        )
    })
}

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
            Some(other) => Err(format!("unknown direction: {}", digest::sanitize(other))),
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
/// Every key goes through [`digest::sanitize`] — a key is graph content,
/// and these lines are line-structured digests like any other.
fn push_key_list(out: &mut String, lead: &str, keys: &[String]) {
    let mut line = lead.to_string();
    let mut empty = line.is_empty();
    for (i, k) in keys.iter().enumerate() {
        let k = digest::sanitize(k);
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
        .map(|t| digest::sanitize(t))
        .collect::<Vec<_>>()
        .join(", ");
    let when = at.map_or_else(String::new, |a| format!(" as of commit {a}"));
    let mut out = format!(
        "mushroomdb {tool} — {}{when} — partners linked by all of {types}: {}\n",
        digest::sanitize(key),
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
        "listed": partners.len().min(limit),
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
    let rule = rule.map_or_else(String::new, |r| format!(", rule {}", digest::sanitize(r)));
    let lead = format!("{} ({edges}{rule}):", digest::sanitize(edge_type));
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
        "listed": partners.len().min(limit),
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

    if !all_of.is_empty() && edge_type.is_some() {
        return CallOutcome::ToolErr(ONE_OF_ALL_OF_OR_EDGE_TYPE.into());
    }

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
            // An empty answer is where a typo and a fact look the same; only
            // there is the census worth a pass.
            if partners.is_empty() {
                if let Some(e) = unknown_edge_type(db, &all_of) {
                    return CallOutcome::ToolErr(e);
                }
            }
            let report = partners_json(key, None, &all_of, label, &partners, limit);
            return ok(json_out, &report, |_| {
                render_all_of("edges", key, None, &all_of, &partners, limit)
            });
        }
        let edge_type = edge_type.unwrap_or_default();
        let (count, partners) = partners_of_type(&rows, edge_type, dir);
        if count == 0 {
            if let Some(e) = unknown_edge_type(db, &[edge_type.to_string()]) {
                return CallOutcome::ToolErr(e);
            }
        }
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
                digest::sanitize(key),
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
        Ok((total, groups)) => ok(
            json_out,
            &edge_groups_json(key, total, &groups, label),
            |_| render_edge_groups(key, total, &groups),
        ),
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
    let label = match opt_str_arg(args, "label") {
        Ok(l) => l,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let filter = (!edge_types.is_empty()).then_some(edge_types.as_slice());

    if depth <= 1 {
        let limit = match edge_limit_arg(args) {
            Ok(n) => n,
            Err(e) => return CallOutcome::ToolErr(e),
        };
        return edge_reply(db, key, filter, dir, limit, label, json_out);
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
        Ok(rs) => CallOutcome::ToolOk(crate::json::result_set_json(&keeping_label(rs, label))),
        Err(e) => CallOutcome::ToolErr(crate::mcp::graph_err_msg(e)),
    }
}

/// A traversal table narrowed to the rows carrying `label`.
///
/// Past one hop the reply is the BFS table, and the table already carries a
/// `label` column — so narrowing it is a filter on rows rather than on the
/// walk. Deliberately not a filter on the *traversal*: a hop through a node of
/// another label is how a two-hop question reaches the label it asked about,
/// and refusing to walk through it would answer a different question. So the
/// walk is whole and the answer is the nodes of that label it reached.
fn keeping_label(rs: core_api::ResultSet, label: Option<&str>) -> core_api::ResultSet {
    let Some(label) = label else {
        return rs;
    };
    let mut out = core_api::ResultSet::new(rs.columns().to_vec());
    for i in 0..rs.len() {
        if rs.get(i, "label") == Some(&Value::Str(label.to_string())) {
            out.push_row(rs.row(i).to_vec());
        }
    }
    out
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
        digest::sanitize(key)
    );
    if groups.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for g in groups {
        out.push_str(&format!(
            "{} ({})\n",
            digest::sanitize(&g.edge_type),
            g.count
        ));
        for e in &g.listed {
            let arrow = if e.outgoing { "→" } else { "←" };
            out.push_str(&format!("  {arrow} {}", digest::sanitize(&e.other)));
            if let Some(rule) = &e.rule {
                out.push_str(&format!("  rule {}", digest::sanitize(rule)));
            }
            out.push('\n');
        }
        if g.count > g.listed.len() {
            out.push_str(&format!("  … and {} more\n", g.count - g.listed.len()));
        }
    }
    cap_grouped(&out)
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
    // `at` takes a commit index or a date. A date is what a caller actually
    // asks — "on 2026-06-19" — and before v0.6.11 there was no way to express
    // it, so an agent had to reconstruct a date→commit map by hand and a wrong
    // guess returned a plausible wrong graph. A string that will not parse is a
    // tool error, never a guessed commit.
    let at = match args.get("at") {
        None | Some(Js::Null) => return CallOutcome::ToolErr("missing at".into()),
        Some(Js::String(date)) => match db.read().resolve_date(date) {
            Ok(c) => c,
            Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
        },
        Some(v) => match v.as_u64() {
            Some(n) => n,
            None => {
                return CallOutcome::ToolErr(
                    "at must be a non-negative commit index or an RFC 3339 date \
                     (\"2026-06-19\", \"2026-06-19T12:00:00Z\")"
                        .into(),
                )
            }
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
    if !all_of.is_empty() && edge_type.is_some() {
        return CallOutcome::ToolErr(ONE_OF_ALL_OF_OR_EDGE_TYPE.into());
    }
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
                digest::sanitize(&self_key)
            );
            render_type_partners(header, edge_type, count, rule.as_deref(), &partners, limit)
        });
    }

    // The grouped view, narrowed by `direction` and by the partners carrying
    // `label` if one was named — counts included, so the header says what the
    // filters left. Both apply here exactly as they do on `node_edges`: an
    // argument the tool accepts has to mean the same thing in every form of
    // its reply.
    let mut filter = LabelFilter::new(db, label);
    let edges: Vec<core_api::EdgeAt> = edges
        .into_iter()
        .filter(|e| {
            let outgoing = e.src_key == self_key;
            let other = if outgoing { &e.dst_key } else { &e.src_key };
            match dir {
                Dir::Out if !outgoing => false,
                Dir::In if outgoing => false,
                _ => filter.keeps(other),
            }
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
            digest::sanitize(&g.edge_type),
            g.count
        ));
        for l in g.lines.iter().take(limit) {
            if l.incident {
                let arrow = if l.outgoing { "→" } else { "←" };
                let other = if l.outgoing { &l.dst } else { &l.src };
                out.push_str(&format!("    {arrow} {}", digest::sanitize(other)));
            } else {
                out.push_str(&format!(
                    "    {} → {}",
                    digest::sanitize(&l.src),
                    digest::sanitize(&l.dst)
                ));
            }
            if let Some(rule) = &l.rule {
                out.push_str(&format!("  rule {}", digest::sanitize(rule)));
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
    // No line budget on top of `limit`. This reply has two sections, and a
    // budget that ran out inside the first one deleted the second without
    // saying so: at `limit: 50` with fifty lost edges, the whole `gained`
    // section — heading included — fell off the end of a reply that had just
    // said how many there were. `limit` is the cap here, and every group that
    // it cuts says `… and N more` itself.
    out
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
        digest::sanitize(key),
        digest::sanitize(field),
        digest::sanitize(&value.to_string()),
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
            .map_or_else(String::new, |r| format!(", rule {}", digest::sanitize(r)));
        let lead = format!("{} ({}{rule}):", digest::sanitize(edge_type), side.len());
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
            digest::sanitize(&raw.to_string())
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
    let outcome = {
        let g = db.read();
        core_api::memory::recall::recall_digest(&*g, &topic, &label, MAX_OUTPUT_BYTES)
    };
    let (digest, text) = match outcome {
        core_api::memory::recall::RecallOutcome::Hits(d) => (d.clone(), d),
        core_api::memory::recall::RecallOutcome::NoMatch => (
            String::new(),
            format!(
                "mushroomdb recall — nothing matches {}\n",
                digest::sanitize(&topic)
            ),
        ),
        // Not the same answer as "no match": nothing here can ever match, and
        // the caller can fix that. Names the real store path — `db_dir` is
        // already in hand as `label` above — rather than a literal `<db>`
        // placeholder the caller has to translate themselves; `label` falls
        // back to the word "store" on the one path with no directory to
        // name (the in-process `SharedDb` case, `db_dir: None`), where that
        // really is the best available answer.
        core_api::memory::recall::RecallOutcome::NoIndex => (
            String::new(),
            format!(
                "mushroomdb recall — this store has no text index, so no topic can \
                 match. Run `mushroomdb schema apply {label} --memory-defaults`.\n"
            ),
        ),
    };
    ok(
        json_out,
        &json!({ "topic": topic, "digest": digest }),
        |_| text,
    )
}

// ── remember ─────────────────────────────────────────────────────────────────

/// Parse `args.entities` into [`EntityIn`]s. Absent/null is an empty list;
/// anything else must be an array of `{key, label, props?}` objects.
fn entities_arg(args: &Js) -> Result<Vec<EntityIn>, String> {
    let items = match args.get("entities") {
        None | Some(Js::Null) => return Ok(Vec::new()),
        Some(Js::Array(items)) => items,
        Some(_) => return Err("entities must be an array".into()),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(obj) = item.as_object() else {
            return Err("entities[] must be objects".into());
        };
        let Some(key) = obj.get("key").and_then(Js::as_str) else {
            return Err("entities[].key is required".into());
        };
        let Some(label) = obj.get("label").and_then(Js::as_str) else {
            return Err("entities[].label is required".into());
        };
        let mut props = BTreeMap::new();
        if let Some(props_obj) = obj.get("props").and_then(Js::as_object) {
            for (field, json_val) in props_obj {
                match json_to_value(json_val.clone()) {
                    Some(v) => {
                        props.insert(field.clone(), v);
                    }
                    None => {
                        return Err(format!(
                            "entities[].props.{field} is not a supported value type"
                        ))
                    }
                }
            }
        }
        out.push(EntityIn {
            key: key.to_string(),
            label: label.to_string(),
            props,
        });
    }
    Ok(out)
}

/// Parse `args.facts` into [`FactIn`]s. Absent/null is an empty list; anything
/// else must be an array of `{subject, predicate, object}` objects.
fn facts_arg(args: &Js) -> Result<Vec<FactIn>, String> {
    let items = match args.get("facts") {
        None | Some(Js::Null) => return Ok(Vec::new()),
        Some(Js::Array(items)) => items,
        Some(_) => return Err("facts must be an array".into()),
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(obj) = item.as_object() else {
            return Err("facts[] must be objects".into());
        };
        let (Some(subject), Some(predicate), Some(object)) = (
            obj.get("subject").and_then(Js::as_str),
            obj.get("predicate").and_then(Js::as_str),
            obj.get("object").and_then(Js::as_str),
        ) else {
            return Err("facts[] requires subject, predicate, and object".into());
        };
        out.push(FactIn {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
        });
    }
    Ok(out)
}

fn tool_remember(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let text = match str_arg(args, "text") {
        Ok(t) => t.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let mut about = match str_list_arg(args, "about") {
        Ok(a) => a,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    // Deduped, order kept — the order `remember` reports `provisional` keys in.
    {
        let mut seen = BTreeSet::new();
        about.retain(|k| seen.insert(k.clone()));
    }
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
    let source = match opt_str_arg(args, "source") {
        Ok(s) => s,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let ts = match args.get("ts") {
        None | Some(Js::Null) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
        Some(v) => match v.as_i64() {
            Some(n) => n,
            None => return CallOutcome::ToolErr("ts must be an integer".into()),
        },
    };
    let entities = match entities_arg(args) {
        Ok(e) => e,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let facts = match facts_arg(args) {
        Ok(f) => f,
        Err(e) => return CallOutcome::ToolErr(e),
    };

    let input = RememberInput {
        text: &text,
        about: &about,
        kind: &kind,
        ts,
        source,
        entities: &entities,
        facts: &facts,
    };
    let result = {
        let mut g = db.write();
        remember(&mut *g, &input)
    };
    match result {
        Ok(report) => {
            let mut rendered = format!("remembered {}\n", digest::sanitize(&report.note));
            if report.created > 0 || report.matched > 0 {
                rendered.push_str(&format!(
                    "entities  {} created, {} matched\n",
                    report.created, report.matched
                ));
            }
            if report.derived > 0 {
                rendered.push_str(&format!("derived   {} edge(s)\n", report.derived));
            }
            if !report.provisional.is_empty() {
                rendered.push_str(&format!(
                    "provisional  {} — named but not yet described\n",
                    report
                        .provisional
                        .iter()
                        .map(|k| digest::sanitize(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !report.provisional_capped.is_empty() {
                rendered.push_str(&format!(
                    "REFUSED  {} — the per-commit provisional cap was already spent; \
                     these were NOT created and have no edge in this note\n",
                    report
                        .provisional_capped
                        .iter()
                        .map(|k| digest::sanitize(k))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !report.fulltext_declared.is_empty() {
                rendered.push_str(&format!(
                    "indexed  {} — full-text search enabled for the first time\n",
                    report.fulltext_declared.join(", ")
                ));
            }
            ok(
                json_out,
                &json!({
                    "key": report.note,
                    "kind": kind,
                    "about": about,
                    "created": report.created,
                    "matched": report.matched,
                    "derived": report.derived,
                    "provisional": report.provisional,
                    "provisional_capped": report.provisional_capped,
                    "fulltext_declared": report.fulltext_declared,
                }),
                |_| rendered,
            )
        }
        Err(e) => CallOutcome::ToolErr(match e {
            GraphError::QueryError { detail } | GraphError::IngestError { detail } => detail,
            other => other.to_string(),
        }),
    }
}

// ── schema ───────────────────────────────────────────────────────────────────

/// How long `schema` may spend counting.
///
/// The brief's own default is three seconds because it re-reads the WAL, and
/// this call holds a read guard for that long, so a writer in the same process
/// waits behind it. One second is a third of that and, measured on a
/// 100,000-node store in a release build, five times what the whole report
/// takes (188 ms). A spent budget is reported, never silent.
const SCHEMA_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

fn tool_schema(db: &SharedDb, json_out: bool) -> CallOutcome {
    let report = {
        let g = db.read();
        core_api::memory::schema::schema_report(
            &g,
            &core_api::memory::brief::BriefOptions {
                budget: SCHEMA_BUDGET,
            },
        )
    };
    ok(json_out, &report, core_api::memory::schema::render_schema)
}

// ── forget ───────────────────────────────────────────────────────────────────

/// Notes a `forget` reply names when what it forgot was written about; the
/// rest are counted.
const FORGET_NOTE_LIST: usize = 10;

/// The refusal for a call that is not exactly one of the three shapes.
const FORGET_SHAPE: &str = "pass exactly one of: key (forget a node), key and prop \
     (forget one property), or fact {subject, predicate, object} (retract one edge)";

/// What one `forget` call did.
#[derive(serde::Serialize)]
struct ForgetReport {
    /// `node`, `prop` or `fact`.
    mode: &'static str,
    /// The node, property or edge named, as the caller named it.
    target: String,
    /// False when there was nothing to forget, and nothing was written.
    changed: bool,
    /// Edges removed with a node: written by hand, and derived by rules. For a
    /// property, `derived_edges` counts the rule-derived edges on the node that
    /// the removal retracted, because a rule read that property.
    manual_edges: u64,
    derived_edges: u64,
    /// The property removed, in `prop` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    prop: Option<String>,
    /// Notes with an `ABOUT` edge to the node, or to both ends of the fact.
    /// Listed, never deleted: their text still says what was forgotten.
    notes: Vec<String>,
    notes_total: usize,
    /// The first commit history still answers from.
    history_floor: u64,
}

/// Every field a predicate reads, its parts' included, sorted and deduped.
fn predicate_fields(p: &core_api::PredicateSummary) -> Vec<String> {
    let mut out: BTreeSet<String> = p.fields.iter().cloned().collect();
    for part in p.parts.iter().flatten() {
        out.extend(predicate_fields(part));
    }
    out.into_iter().collect()
}

/// Notes with an `ABOUT` edge to every one of `keys`, sorted.
fn notes_about(g: &core_api::GraphDb<core_api::RealFs>, keys: &[&str]) -> Vec<String> {
    let mut common: Option<BTreeSet<String>> = None;
    for key in keys {
        let into: BTreeSet<String> = g
            .neighbors(key, "ABOUT", core_api::Direction::In)
            .unwrap_or_default()
            .into_iter()
            .filter(|k| g.node_ref(k).is_some_and(|n| n.label() == "Note"))
            .collect();
        common = Some(match common {
            None => into,
            Some(c) => c.intersection(&into).cloned().collect(),
        });
    }
    common.unwrap_or_default().into_iter().collect()
}

/// An edge as `(edge_type, src_key, dst_key)`.
type EdgeTriple = (String, String, String);

/// The rule-derived edges incident on `key`, both directions, each once.
fn derived_edges_on(
    g: &core_api::GraphDb<core_api::RealFs>,
    key: &str,
) -> Result<BTreeSet<EdgeTriple>, GraphError> {
    Ok(g.node_edges(key)?
        .into_iter()
        .filter(|e| e.derived)
        .map(|e| (e.edge_type, e.src_key, e.dst_key))
        .collect())
}

/// The history sentence every successful `forget` ends with. A tombstone is
/// not a redaction, and the caller is told so in the same reply.
fn history_line(floor: u64) -> String {
    format!(
        "history still holds it: node_history, edge_history, edges_at and was_linked \
         read it back from commit {floor} on, until `mushroomdb migrate`, \
         `snapshot --truncate` or `--retention` prunes the log\n"
    )
}

fn render_forget(r: &ForgetReport) -> String {
    let target = digest::sanitize(&r.target);
    let mut out = match (r.mode, r.changed) {
        ("node", _) => format!(
            "forgot {target} — {} edge(s) removed, {} derived edge(s) retracted\n",
            r.manual_edges, r.derived_edges
        ),
        ("prop", true) => {
            let mut line = format!("forgot {target}\n");
            if r.derived_edges > 0 {
                line.push_str(&format!(
                    "{} derived edge(s) retracted because a rule read {}\n",
                    r.derived_edges,
                    digest::sanitize(r.prop.as_deref().unwrap_or_default())
                ));
            }
            line
        }
        ("prop", false) => format!("{target} is not set; nothing to forget\n"),
        (_, true) => format!("retracted {target}\n"),
        (_, false) => format!("no edge {target}; nothing to retract\n"),
    };
    if r.notes_total > 0 {
        let named: Vec<String> = r.notes.iter().map(|k| digest::sanitize(k)).collect();
        let more = r.notes_total - named.len();
        out.push_str(&format!(
            "{} note(s) still say it — their text is unchanged and recall can return them; \
             forget a note by its key: {}{}\n",
            r.notes_total,
            named.join(", "),
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        ));
    }
    if r.changed {
        out.push_str(&history_line(r.history_floor));
    }
    out
}

/// A `fact` argument: three non-empty strings.
fn fact_arg(v: &Js) -> Result<(String, String, String), String> {
    let field = |name: &str| {
        v.get(name)
            .and_then(Js::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("fact.{name} must be a non-empty string"))
    };
    Ok((field("subject")?, field("predicate")?, field("object")?))
}

fn tool_forget(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let key = match opt_str_arg(args, "key") {
        Ok(k) => k.map(str::to_string),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let prop = match opt_str_arg(args, "prop") {
        Ok(p) => p.map(str::to_string),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let fact = match args.get("fact") {
        None | Some(Js::Null) => None,
        Some(v) if v.is_object() => match fact_arg(v) {
            Ok(f) => Some(f),
            Err(e) => return CallOutcome::ToolErr(e),
        },
        Some(_) => return CallOutcome::ToolErr("fact must be an object".into()),
    };
    // One write guard for the reads the reply needs and the write itself, so
    // nothing can change between what the reply says and what was done.
    let mut g = db.write();
    let report = match (key, prop, fact) {
        (Some(key), None, None) => {
            if !g.has_node(&key) {
                return CallOutcome::ToolErr(graph_err_msg(GraphError::KeyNotFound { key }));
            }
            let notes = notes_about(&g, &[key.as_str()]);
            let label = g
                .node_ref(&key)
                .map(|n| n.label().to_string())
                .unwrap_or_default();
            let deleted = match g.delete_node(&key) {
                Ok(r) => r,
                Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
            };
            ForgetReport {
                mode: "node",
                target: format!("{key} ({label})"),
                changed: true,
                manual_edges: deleted.manual_edges,
                derived_edges: deleted.derived_edges,
                prop: None,
                notes_total: notes.len(),
                notes: notes.into_iter().take(FORGET_NOTE_LIST).collect(),
                history_floor: g.stats().history_floor,
            }
        }
        (Some(key), Some(prop), None) => {
            // Removing a property re-runs rule retraction, so a rule that read
            // it drops the edges it derived. Compare the node's derived edges
            // either side of the write to say how many went.
            let before = match derived_edges_on(&g, &key) {
                Ok(e) => e,
                Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
            };
            let changed = match g.remove_prop(&key, &prop) {
                Ok(c) => c,
                Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
            };
            let retracted = if changed {
                let after = derived_edges_on(&g, &key).unwrap_or_default();
                before.difference(&after).count() as u64
            } else {
                0
            };
            ForgetReport {
                mode: "prop",
                target: format!("{key}.{prop}"),
                changed,
                manual_edges: 0,
                derived_edges: retracted,
                prop: Some(prop),
                notes: Vec::new(),
                notes_total: 0,
                history_floor: g.stats().history_floor,
            }
        }
        (None, None, Some((subject, predicate, object))) => {
            let target = format!("{predicate} {subject} → {object}");
            let changed = match g.delete_edge(&predicate, &subject, &object) {
                Ok(c) => c,
                Err(GraphError::RuleOwned { detail }) => {
                    return CallOutcome::ToolErr(rule_owned_refusal(
                        &g, &predicate, &subject, &object, &detail,
                    ))
                }
                Err(e) => return CallOutcome::ToolErr(graph_err_msg(e)),
            };
            let notes = if changed {
                notes_about(&g, &[subject.as_str(), object.as_str()])
            } else {
                Vec::new()
            };
            ForgetReport {
                mode: "fact",
                target,
                changed,
                manual_edges: 0,
                derived_edges: 0,
                prop: None,
                notes_total: notes.len(),
                notes: notes.into_iter().take(FORGET_NOTE_LIST).collect(),
                history_floor: g.stats().history_floor,
            }
        }
        _ => return CallOutcome::ToolErr(FORGET_SHAPE.into()),
    };
    drop(g);
    ok(json_out, &report, render_forget)
}

/// Why a fact edge cannot be retracted: which rule owns it, and the only two
/// ways it can change.
fn rule_owned_refusal(
    g: &core_api::GraphDb<core_api::RealFs>,
    predicate: &str,
    subject: &str,
    object: &str,
    detail: &str,
) -> String {
    let label = |k: &str| g.node_ref(k).map(|n| n.label().to_string());
    let (src, dst) = (label(subject), label(object));
    let owners: Vec<core_api::RuleDef> = g
        .rules()
        .into_iter()
        .filter(|r| {
            r.edge_type == predicate
                && Some(&r.src_label) == src.as_ref()
                && Some(&r.dst_label) == dst.as_ref()
        })
        .collect();
    if owners.is_empty() {
        return digest::sanitize(detail);
    }
    let names: Vec<String> = owners.iter().map(|r| digest::sanitize(&r.name)).collect();
    let mut fields: BTreeSet<String> = BTreeSet::new();
    for r in &owners {
        fields.extend(predicate_fields(&core_api::PredicateSummary::from(
            &r.predicate,
        )));
    }
    let fields: Vec<String> = fields.iter().map(|f| digest::sanitize(f)).collect();
    format!(
        "refused: {} {} → {} is derived by rule {}. It changes only when the fields \
         that rule reads change ({}), or when the rule is deleted. Nothing was written.",
        digest::sanitize(predicate),
        digest::sanitize(subject),
        digest::sanitize(object),
        names.join(", "),
        fields.join(", ")
    )
}

// ── suggest_rules ────────────────────────────────────────────────────────────

/// Proposals one reply lists.
///
/// Every one is relayed to a person for approval with its estimate, its
/// examples and the exact `create_rule` arguments; five is what one message
/// can carry with all of that, and the reply counts the rest.
const MAX_SUGGESTIONS: usize = 5;

/// Fields the store writes for its own bookkeeping, never proposed as a rule.
///
/// On a memory store these are the whole of what the engine would otherwise
/// propose: measured on a 100,000-node store `remember` filled, every proposal
/// was `kind`, `source` or `ts` — clique rules that link every note to 32
/// others for sharing a default value. `ns` is the namespace every namespaced
/// node carries, `id` repeats the key, `provisional` is `remember`'s stub mark
/// and `aliases` is the list `remember` maintains for identity matching.
const BOOKKEEPING_FIELDS: [&str; 7] =
    ["ns", "kind", "ts", "source", "provisional", "id", "aliases"];

/// One proposal, with the arguments that create it.
#[derive(serde::Serialize)]
struct SuggestionOut {
    name: String,
    src_label: String,
    dst_label: String,
    edge_type: String,
    predicate: String,
    est_edges: u64,
    examples: Vec<(String, String, f64)>,
    rationale: String,
    /// Pass this object to `create_rule` as its arguments, unchanged.
    create_rule_args: Js,
}

#[derive(serde::Serialize)]
struct SuggestOut {
    suggestions: Vec<SuggestionOut>,
    /// Proposals after the bookkeeping filter, before the cap.
    total: usize,
    /// Proposals dropped because they read a bookkeeping field.
    bookkeeping_hidden: usize,
    /// The engine's time budget ran out; a second call may find more.
    truncated: bool,
}

/// `def` as `create_rule` arguments: no nulls, `approximate` only when set,
/// and `weight_prop` explicit — `create_rule` would default a missing one to
/// `"weight"`, so it is written out to make what is shown what is created.
fn create_rule_args(def: &core_api::RuleDef) -> Js {
    let mut v = serde_json::to_value(def).unwrap_or(Js::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|_, x| !x.is_null());
        if obj.get("approximate") == Some(&Js::Bool(false)) {
            obj.remove("approximate");
        }
        obj.entry("weight_prop").or_insert_with(|| json!("weight"));
    }
    v
}

fn render_suggestions(r: &SuggestOut) -> String {
    let mut out = if r.suggestions.is_empty() {
        "mushroomdb suggest_rules — nothing to propose: no pattern a rule does not \
         already cover\n"
            .to_string()
    } else {
        format!(
            "mushroomdb suggest_rules — {} proposal(s); nothing is created until \
             create_rule is called with the arguments shown\n",
            r.total
        )
    };
    for (i, s) in r.suggestions.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}: {} → {} derives {} — {} · ~{} edge(s) · global: links across namespaces\n",
            i + 1,
            digest::sanitize(&s.name),
            digest::sanitize(&s.src_label),
            digest::sanitize(&s.dst_label),
            digest::sanitize(&s.edge_type),
            s.predicate,
            s.est_edges
        ));
        out.push_str(&format!("   why: {}\n", digest::sanitize(&s.rationale)));
        if !s.examples.is_empty() {
            let eg: Vec<String> = s
                .examples
                .iter()
                .map(|(a, b, score)| {
                    format!(
                        "{} → {} {score:.2}",
                        digest::sanitize(a),
                        digest::sanitize(b)
                    )
                })
                .collect();
            out.push_str(&format!("   e.g. {}\n", eg.join("; ")));
        }
        out.push_str(&format!(
            "   create_rule {}\n",
            digest::sanitize(&s.create_rule_args.to_string())
        ));
    }
    if r.total > r.suggestions.len() {
        out.push_str(&format!("… and {} more\n", r.total - r.suggestions.len()));
    }
    if r.bookkeeping_hidden > 0 {
        out.push_str(&format!(
            "({} proposal(s) on bookkeeping fields — {} — not shown)\n",
            r.bookkeeping_hidden,
            BOOKKEEPING_FIELDS.join(", ")
        ));
    }
    if r.truncated {
        out.push_str("(the time budget ran out; a second call may find more)\n");
    }
    out
}

fn tool_suggest_rules(db: &SharedDb, json_out: bool) -> CallOutcome {
    let report = {
        let g = db.read();
        g.suggest_rules_with_config(
            &core_api::SuggestConfig::default(),
            core_api::SUGGEST_DEFAULT_SEED,
        )
    };
    let mut kept: Vec<SuggestionOut> = Vec::new();
    let mut hidden = 0usize;
    for s in report.suggestions {
        let summary = core_api::PredicateSummary::from(&s.def.predicate);
        if predicate_fields(&summary)
            .iter()
            .any(|f| BOOKKEEPING_FIELDS.contains(&f.as_str()))
        {
            hidden += 1;
            continue;
        }
        kept.push(SuggestionOut {
            create_rule_args: create_rule_args(&s.def),
            predicate: predicate_summary(&summary),
            name: s.def.name,
            src_label: s.def.src_label,
            dst_label: s.def.dst_label,
            edge_type: s.def.edge_type,
            est_edges: s.est_edges,
            examples: s.examples,
            rationale: s.rationale,
        });
    }
    let total = kept.len();
    kept.truncate(MAX_SUGGESTIONS);
    let out = SuggestOut {
        suggestions: kept,
        total,
        bookkeeping_hidden: hidden,
        truncated: report.truncated,
    };
    ok(json_out, &out, render_suggestions)
}

// ── analyze ──────────────────────────────────────────────────────────────────

/// Rows `analyze` lists when the caller names no `top`, and the most it will.
const ANALYZE_DEFAULT_TOP: usize = 10;
const ANALYZE_MAX_TOP: usize = 50;

/// Member keys shown per component or cluster; the rest are counted.
const ANALYZE_SAMPLE_MEMBERS: usize = 5;

/// Characters of one key `analyze` prints before cutting it with `…`.
const ANALYZE_KEY_CHARS: usize = 80;

/// The kinds `analyze` answers.
const ANALYZE_KINDS: [&str; 4] = ["central", "clusters", "components", "degree"];

fn top_arg(args: &Js) -> Result<usize, String> {
    match args.get("top") {
        None | Some(Js::Null) => Ok(ANALYZE_DEFAULT_TOP),
        Some(v) => match v.as_u64() {
            Some(0) | None => Err("top must be a positive integer".into()),
            Some(n) => Ok(usize::try_from(n)
                .unwrap_or(ANALYZE_MAX_TOP)
                .min(ANALYZE_MAX_TOP)),
        },
    }
}

/// A key as `analyze` prints it: sanitized and cut at [`ANALYZE_KEY_CHARS`].
fn shown_key(key: &str) -> String {
    let s = digest::sanitize(key);
    if s.chars().count() <= ANALYZE_KEY_CHARS {
        return s;
    }
    let mut cut: String = s.chars().take(ANALYZE_KEY_CHARS).collect();
    cut.push('…');
    cut
}

/// `members` as a sample line: the first few keys, and how many more.
fn sample(members: &[String]) -> String {
    let shown: Vec<String> = members
        .iter()
        .take(ANALYZE_SAMPLE_MEMBERS)
        .map(|k| shown_key(k))
        .collect();
    let more = members.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} (+{more} more)", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// One ranked node line: rank, key, label, value, and its summary when it has one.
fn node_row(
    g: &core_api::GraphDb<core_api::RealFs>,
    rank: usize,
    key: &str,
    value: &str,
) -> (String, Js) {
    let label = g
        .node_ref(key)
        .map(|n| n.label().to_string())
        .unwrap_or_default();
    let summary = g.node_summary_line(key);
    let mut line = format!(
        "{rank:>3}. {} [{}] {value}",
        shown_key(key),
        digest::sanitize(&label)
    );
    if let Some(s) = &summary {
        line.push_str(&format!(" — {}", digest::sanitize(s)));
    }
    line.push('\n');
    (
        line,
        json!({ "key": key, "label": label, "value": value, "summary": summary }),
    )
}

fn tool_analyze(db: &SharedDb, args: &Js, json_out: bool) -> CallOutcome {
    let kind = match str_arg(args, "kind") {
        Ok(k) => k.to_string(),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    if !ANALYZE_KINDS.contains(&kind.as_str()) {
        return CallOutcome::ToolErr(format!(
            "kind must be one of {}, got {:?}",
            ANALYZE_KINDS.join(", "),
            kind
        ));
    }
    let top = match top_arg(args) {
        Ok(t) => t,
        Err(e) => return CallOutcome::ToolErr(e),
    };
    let edge_type = match opt_str_arg(args, "edge_type") {
        Ok(t) => t.map(str::to_string),
        Err(e) => return CallOutcome::ToolErr(e),
    };
    if let Some(t) = &edge_type {
        if let Some(msg) = unknown_edge_type(db, std::slice::from_ref(t)) {
            return CallOutcome::ToolErr(msg);
        }
    }
    let over = edge_type.as_deref().map_or_else(
        || "every edge type".to_string(),
        |t| format!("{} edges", digest::sanitize(t)),
    );
    // `budget_ms: 0` everywhere: a wall-clock budget makes the answer depend on
    // the machine's load, and the same question must get the same answer.
    // Measured on a 100,000-node, 180,000-edge store in a release build, the
    // slowest of the four ran in 88 ms unbudgeted.
    let g = db.read();
    let (text, doc) = match kind.as_str() {
        "central" | "degree" => {
            let (scores, header): (Vec<(String, String)>, String) = if kind == "central" {
                let r = g.pagerank(&core_api::PageRankConfig {
                    edge_type: edge_type.clone(),
                    budget_ms: 0,
                    ..Default::default()
                });
                let note = if r.converged {
                    "converged"
                } else {
                    "stopped at 50 iterations"
                };
                (
                    r.scores
                        .iter()
                        .map(|(k, s)| (k.clone(), format!("{s:.6}")))
                        .collect(),
                    format!("PageRank over {over}, {note}"),
                )
            } else {
                let r = g.degree_centrality(&core_api::DegreeConfig {
                    edge_type: edge_type.clone(),
                    budget_ms: 0,
                    ..Default::default()
                });
                (
                    r.scores
                        .iter()
                        .map(|(k, d)| (k.clone(), format!("degree {d}")))
                        .collect(),
                    format!("degree over {over}, in and out"),
                )
            };
            let mut text = format!(
                "mushroomdb analyze {kind} — {header}; {} node(s), top {}\n",
                scores.len(),
                top.min(scores.len())
            );
            let mut rows = Vec::new();
            for (i, (k, v)) in scores.iter().take(top).enumerate() {
                let (line, row) = node_row(&g, i + 1, k, v);
                text.push_str(&line);
                rows.push(row);
            }
            (
                text,
                json!({ "kind": kind, "nodes": scores.len(), "listed": rows.len(), "rows": rows }),
            )
        }
        "components" => {
            let r = g.connected_components(&core_api::WccConfig {
                edge_type: edge_type.clone(),
                budget_ms: 0,
                ..Default::default()
            });
            let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
            for (key, comp) in &r.components {
                groups.entry(comp.as_str()).or_default().push(key.clone());
            }
            let mut groups: Vec<Vec<String>> = groups.into_values().collect();
            groups.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
            let singletons = groups.iter().filter(|g| g.len() == 1).count();
            let mut text = format!(
                "mushroomdb analyze components — {} component(s) over {} node(s) by {over}; \
                 largest {}; {singletons} singleton(s)\n",
                groups.len(),
                r.components.len(),
                groups.first().map_or(0, Vec::len)
            );
            let mut rows = Vec::new();
            for (i, members) in groups.iter().filter(|g| g.len() > 1).take(top).enumerate() {
                text.push_str(&format!(
                    "{:>3}. size {} — {}\n",
                    i + 1,
                    members.len(),
                    sample(members)
                ));
                rows.push(json!({
                    "size": members.len(),
                    "members": members.iter().take(ANALYZE_SAMPLE_MEMBERS).collect::<Vec<_>>()
                }));
            }
            (
                text,
                json!({
                    "kind": kind, "nodes": r.components.len(), "components": groups.len(),
                    "singletons": singletons, "listed": rows.len(), "rows": rows
                }),
            )
        }
        _ => {
            let r = g.communities(&core_api::LouvainConfig {
                edge_types: edge_type.clone().into_iter().collect(),
                budget_ms: 0,
                ..Default::default()
            });
            let nodes: usize = r.communities.iter().map(|c| c.members.len()).sum();
            let (multi, single): (Vec<_>, Vec<_>) =
                r.communities.iter().partition(|c| c.members.len() > 1);
            let mut text = format!(
                "mushroomdb analyze clusters — {} cluster(s) of two or more over {nodes} node(s) \
                 by {over}, modularity {:.3}; {} singleton(s) not listed\n",
                multi.len(),
                r.modularity,
                single.len()
            );
            let mut rows = Vec::new();
            for (i, c) in multi.iter().take(top).enumerate() {
                text.push_str(&format!(
                    "{:>3}. size {}, cohesion {:.2} — {}\n",
                    i + 1,
                    c.members.len(),
                    c.cohesion,
                    sample(&c.members)
                ));
                rows.push(json!({
                    "size": c.members.len(),
                    "cohesion": c.cohesion,
                    "members": c.members.iter().take(ANALYZE_SAMPLE_MEMBERS).collect::<Vec<_>>()
                }));
            }
            (
                text,
                json!({
                    "kind": kind, "nodes": nodes, "clusters": multi.len(),
                    "singletons": single.len(), "modularity": r.modularity,
                    "listed": rows.len(), "rows": rows
                }),
            )
        }
    };
    drop(g);
    ok(json_out, &doc, |_| text)
}

// ── tools/list ───────────────────────────────────────────────────────────────

/// The `json` argument every task tool takes, added to every task tool's schema
/// by [`task_tools`] rather than written out in each.
fn json_arg() -> Js {
    json!({
        "type": "boolean",
        "description": "Answer with the report as JSON, not the rendered digest."
    })
}

/// The task tools, in the order `tools/list` puts them: the question an
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
            "description": "What is K related to — every relationship of one node, grouped by edge type with a count, each listed edge carrying its direction, the rule that derived it, its score and the predicate it matched on. Answers 'why is this here' in the same call that lists it, so no follow-up explain is needed. Which partners are linked by all of these types? pass all_of and the reply is just their keys; pass one edge_type for that type's partner keys with the rule named once. label narrows partners. With json:true the report carries `listed` and `total`, so a reply that was cut still says how much there was.",
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
                        "description": "Edges listed per edge type (default 10, max 100), or partner keys listed under edge_type/all_of (default 200, max 2000). The rest are counted as '… and N more'."
                    }
                },
                "required": ["key"]
            }
        }),
        json!({
            "name": "neighborhood",
            "description": "What is around K — one hop out, as the same grouped relationship listing node_edges gives, with the rule and score behind each edge. With depth above 1 it is the breadth-first table of (key, label, depth) instead, because past one hop no single rule accounts for a row. label narrows the reply to nodes carrying it, at either depth.",
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
                    "label": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Only nodes carrying this label: at depth 1 the partners, counts included; above it the rows of the traversal table. The walk itself is never narrowed — a hop through another label is how the label you asked about is reached."
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
            "description": "What did K's relationships look like on DATE (or at commit C) — the edges that were live at one point in the store's history, with the rule that had derived each. `at` takes EITHER an RFC 3339 date (\"2026-06-19\", or \"2026-06-19T12:00:00Z\") OR a 0-based WAL commit index. Pass the date directly: it resolves to the last commit at or before that instant. Do NOT guess a commit index for a date, and do not go probing commits to find one — a store that cannot answer a date says so by name. Which partners were linked by all of these types on that day? pass all_of and the reply is just their keys; pass one edge_type for that type's partner keys with the rule named once. label narrows partners. With json:true the report carries `listed` and `total`, so a reply that was cut still says how much there was.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "Node key." },
                    "at": {
                        "description": "When to read the edges. Either an RFC 3339 date string — \"2026-06-19\", \"2026-06-19T12:00:00Z\", offsets accepted — which resolves to the last commit at or before that instant, or a 0-based WAL commit index. Prefer the date when the question names one.",
                        "anyOf": [
                            { "type": "string", "minLength": 1 },
                            { "type": "integer", "minimum": 0 }
                        ]
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
            "description": "What do I already know about this — ranked nodes matching a free-text topic across every indexed text field, with one line each.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Free-form text. A question, a name, or a phrase."
                    }
                },
                "required": ["topic"]
            }
        }),
        json!({
            "name": "remember",
            "description": "Remember this for next time — write a note into the graph and return what it \
        did. Use 'about' for a subject whose type you don't know yet — an unknown key there is \
        created as a provisional entity (label 'Entity'), not refused, and stays that label \
        permanently until it is named through 'entities' instead. Use 'entities' for a subject \
        whose type you DO know: it is created (or described, if it already exists) under the \
        label you give it, correctly labelled the first time — the label can never be changed \
        afterward, not even by upsert_entity, so name it here when you can rather than leaving it \
        for 'about' to guess at. Pass 'facts' for the relationships you recognised, between keys \
        named in 'entities' or 'about', in the same commit.",
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
                        "description": "Node keys the note is about, when you don't know their type. A key that does not exist yet is created as a provisional entity (label 'Entity') — permanently: prefer 'entities' when you know the label, since a provisional node's label can never change later."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["note", "decision", "todo"],
                        "description": "What kind of note this is (default: note)."
                    },
                    "source": {
                        "type": "string",
                        "description": "Where this came from — a session id, a file, a person. Defaults to \"agent\"."
                    },
                    "ts": {
                        "type": "integer",
                        "description": "Unix seconds the fact dates from. Defaults to now. Pass it when importing."
                    },
                    "entities": {
                        "type": "array",
                        "description": "Entities you recognised in the text.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "key":   { "type": "string" },
                                "label": { "type": "string" },
                                "props": { "type": "object" }
                            },
                            "required": ["key", "label"]
                        }
                    },
                    "facts": {
                        "type": "array",
                        "description": "Relationships you recognised, between keys named above.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject":   { "type": "string" },
                                "predicate": { "type": "string" },
                                "object":    { "type": "string" }
                            },
                            "required": ["subject", "predicate", "object"]
                        }
                    }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "schema",
            "description": "What's in here — the store's labels with their property names, its edge types and what derives them, every rule with its predicate, the full-text fields recall searches, the equality indexes, and how many provisional nodes remember created. Call it before writing Cypher against a store you have not seen.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "analyze",
            "description": "What matters here, and what clusters — over the whole store, with no role or mask applied. kind 'central' ranks nodes by PageRank, 'degree' by edge count, 'components' groups connected nodes with sizes, 'clusters' finds communities of two or more. 'top' (default 10, at most 50) bounds the rows; 'edge_type' restricts to one type. The same store always gets the same answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["central", "clusters", "components", "degree"],
                        "description": "Which question."
                    },
                    "top": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 50,
                        "description": "Rows to list (default 10)."
                    },
                    "edge_type": {
                        "type": "string",
                        "description": "Only edges of this type."
                    }
                },
                "required": ["kind"]
            }
        }),
        json!({
            "name": "suggest_rules",
            "description": "What relationships are in my data — rules the store proposes from its own values, each with an estimate, examples and the exact create_rule arguments. Nothing is created: show the proposal and wait for approval before calling create_rule. Fields the store writes for bookkeeping are never proposed. Every proposal is global and links across namespaces.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "forget",
            "description": "Forget that — tombstone a node ('key'), remove one property ('key' and 'prop'), or retract one fact edge ('fact': subject, predicate, object). An edge a rule derived is refused with the rule that owns it. Notes that still state what was forgotten are listed, not deleted. History keeps it until the log is pruned, and the reply says so. There is no role check: this server has no auth.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "minLength": 1, "description": "The node to forget, or whose property to forget." },
                    "prop": { "type": "string", "minLength": 1, "description": "With 'key': the one property to remove." },
                    "fact": {
                        "type": "object",
                        "description": "The edge to retract, as remember's facts name it.",
                        "properties": {
                            "subject":   { "type": "string" },
                            "predicate": { "type": "string" },
                            "object":    { "type": "string" }
                        },
                        "required": ["subject", "predicate", "object"]
                    }
                }
            }
        }),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests: helpers these tools share that a `tools/call` transcript cannot reach.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
