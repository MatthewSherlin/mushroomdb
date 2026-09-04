//! `why` — what links two things, with the evidence.
//!
//! A derived edge is a claim, and a claim an assistant is going to act on
//! should come with what it was derived from. So every link this reports
//! carries the rule that wrote it, the score it was written with, and the lines
//! of the repository that make it true: the commits two files share, the line
//! an import sits on, the line a call is made from, the file an author knows
//! both through, the heading a document mentions something under.
//!
//! When no rule links the two at all the answer is the shortest walk between
//! them — see [`shortest_path`](crate::repograph::shortest_path) — and when
//! there is not even one of those, `why` says so rather than implying a
//! connection nobody can name.

use crate::db::GraphDb;
use crate::repograph::facts::{
    commit_fact, evidence_line, list_prop, neighbors, str_prop, CommitFact,
};
use crate::repograph::owners::SHA_LEN;
use crate::repograph::path::{shortest_path, MAX_HOPS, PATH_EDGES};
use crate::repograph::render::{sanitize, ymd};
use crate::Direction;
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::BTreeSet;

/// Lines of evidence kept per link. Three commits say "these two move
/// together"; thirty say it no better and crowd out the next link.
const MAX_EVIDENCE: usize = 3;

/// How `a` and `b` are linked, if they are.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhyReport {
    pub a: String,
    pub b: String,
    /// Every rule-written edge between them, in either direction.
    pub links: Vec<WhyLink>,
    /// `(edge type, node reached)` hops, filled only when no rule links them
    /// directly and a walk of at most [`MAX_HOPS`] edges connects them.
    pub path: Vec<(String, String)>,
    /// Whichever of `a` and `b` the store has never heard of.
    pub unknown: Vec<String>,
}

/// One rule-written edge, and what makes it true.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhyLink {
    /// The rule that wrote the edge.
    pub rule: String,
    pub edge_type: String,
    /// `a→b` or `b→a`, in terms of the keys the caller asked about.
    pub direction: String,
    /// The score the rule matched at. `None` for a rule that records none.
    pub score: Option<f64>,
    /// For a via-hop rule, the edge type the rule hopped over to find its
    /// candidates. The evidence names the node it hopped through.
    pub via: Option<String>,
    /// The repository facts behind the edge, strongest or newest first.
    pub evidence: Vec<String>,
}

/// Why `a` and `b` are linked.
///
/// Deterministic: links come back in the engine's `(rule, edge type)` order,
/// evidence is sorted before it is cut, and the path — when there is one — is
/// the same shortest walk every time.
#[must_use]
pub fn why<F: Fs>(db: &GraphDb<F>, a: &str, b: &str) -> WhyReport {
    let mut report = WhyReport {
        a: sanitize(a),
        b: sanitize(b),
        links: Vec::new(),
        path: Vec::new(),
        unknown: Vec::new(),
    };
    for key in [a, b] {
        if !db.has_node(key) {
            report.unknown.push(sanitize(key));
        }
    }
    report.unknown.sort();
    report.unknown.dedup();
    if !report.unknown.is_empty() {
        return report;
    }

    for e in db.explain(a, b).unwrap_or_default() {
        let forward = e.src_key == a;
        report.links.push(WhyLink {
            evidence: evidence(db, &e.edge_type, &e.src_key, &e.dst_key),
            rule: sanitize(&e.rule),
            edge_type: sanitize(&e.edge_type),
            direction: if forward { "a→b" } else { "b→a" }.to_string(),
            score: e.weight,
            via: e.via_edge.as_deref().map(sanitize),
        });
    }
    // The engine returns provenance in its own stable order; sorting on the
    // fields themselves means the answer depends on what the links *are*
    // rather than on the order two stores happened to intern their ids in.
    report.links.sort_by(|x, y| {
        x.direction
            .cmp(&y.direction)
            .then(x.edge_type.cmp(&y.edge_type))
            .then(x.rule.cmp(&y.rule))
            .then(
                y.score
                    .partial_cmp(&x.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(x.evidence.cmp(&y.evidence))
    });
    if report.links.is_empty() {
        report.path = shortest_path(db, a, b, &PATH_EDGES, MAX_HOPS);
    }
    report
}

/// The repository facts behind one edge, in the form its kind is read in.
///
/// An edge type nothing is recorded for — an auto-FK edge such as `DEFINES`,
/// whose evidence is the prop it was derived from and is already in the key —
/// gets none, and the digest prints the link alone.
fn evidence<F: Fs>(db: &GraphDb<F>, edge_type: &str, src: &str, dst: &str) -> Vec<String> {
    match edge_type {
        "CO_CHANGED" => shared_commits(db, src, dst),
        "IMPORTS" => match evidence_line(&list_prop(db, src, "import_lines"), dst) {
            Some(line) => vec![sanitize(&format!("{src} line {line}: import {dst}"))],
            None => vec![sanitize(&format!("{src} imports {dst}"))],
        },
        "CALLS" => match evidence_line(&list_prop(db, src, "call_lines"), dst) {
            Some(line) => vec![sanitize(&format!("{src} line {line}: call {dst}"))],
            None => vec![sanitize(&format!("{src} calls {dst}"))],
        },
        "KNOWS" => via_files(db, src, dst),
        "MENTIONS" => vec![mention(db, src, dst)],
        _ => Vec::new(),
    }
}

/// The commits both files were touched by, newest first.
fn shared_commits<F: Fs>(db: &GraphDb<F>, a: &str, b: &str) -> Vec<String> {
    let theirs: BTreeSet<String> = list_prop(db, b, "commits").into_iter().collect();
    let mut shared: Vec<CommitFact> = list_prop(db, a, "commits")
        .into_iter()
        .filter(|sha| theirs.contains(sha))
        .filter_map(|sha| commit_fact(db, &sha))
        .collect();
    shared.sort_by(|x, y| y.ts.cmp(&x.ts).then(x.sha.cmp(&y.sha)));
    shared.dedup_by(|x, y| x.sha == y.sha);
    shared
        .into_iter()
        .take(MAX_EVIDENCE)
        .map(|c| {
            let short: String = c.sha.chars().take(SHA_LEN).collect();
            sanitize(&format!("{short} {} {}", ymd(c.ts), c.subject))
        })
        .collect()
}

/// The files an author knows `file` through: the ones they own that share
/// commits with it, which is what the `knows` rule hopped over to find them.
fn via_files<F: Fs>(db: &GraphDb<F>, author: &str, file: &str) -> Vec<String> {
    let theirs: BTreeSet<String> = list_prop(db, file, "commits").into_iter().collect();
    let mut scored: Vec<(usize, String)> = neighbors(db, author, "TOP_AUTHOR", Direction::In)
        .into_iter()
        .filter(|owned| owned != file)
        .map(|owned| {
            let shared = list_prop(db, &owned, "commits")
                .into_iter()
                .filter(|sha| theirs.contains(sha))
                .count();
            (shared, owned)
        })
        .filter(|(shared, _)| *shared > 0)
        .collect();
    // Most shared commits first, ties on the key: the same order the digest
    // would rank any other association in.
    scored.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
    scored
        .into_iter()
        .take(MAX_EVIDENCE)
        .map(|(shared, owned)| {
            sanitize(&format!(
                "via {owned} ({shared} shared commit{})",
                if shared == 1 { "" } else { "s" }
            ))
        })
        .collect()
}

/// Where in a document a file is mentioned: the heading it sits under.
///
/// The mention itself carries no line, so the line is found in the document's
/// stored `body` — the first one naming the file — and the heading is the
/// nearest one above it. A document whose body was not stored still has its
/// headings, and the first of those is what it is about.
fn mention<F: Fs>(db: &GraphDb<F>, doc: &str, file: &str) -> String {
    let headings = list_prop(db, doc, "headings");
    let nearest = str_prop(db, doc, "body").and_then(|body| {
        let lines: Vec<&str> = body.lines().collect();
        let at = lines.iter().position(|l| l.contains(file))?;
        lines[..=at]
            .iter()
            .rev()
            .find_map(|l| heading_text(l).map(str::to_string))
    });
    match nearest.or_else(|| headings.first().cloned()) {
        Some(heading) => sanitize(&format!("{doc} mentions {file} under \"{heading}\"")),
        None => sanitize(&format!("{doc} mentions {file}")),
    }
}

/// The text of a Markdown ATX heading, if the line is one.
fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed.get(hashes..)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::heading_text;

    #[test]
    fn a_heading_is_hashes_a_space_and_a_title() {
        assert_eq!(heading_text("## Rules"), Some("Rules"));
        assert_eq!(heading_text("  # Top  "), Some("Top"));
        assert_eq!(heading_text("### Closed ###"), Some("Closed"));
        assert_eq!(heading_text("#no-space"), None);
        assert_eq!(heading_text("####### too deep"), None);
        assert_eq!(heading_text("plain text"), None);
        assert_eq!(heading_text("#"), None);
    }
}
