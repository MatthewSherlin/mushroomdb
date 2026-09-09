//! `explore` — the one tool that finds things in a code graph.
//!
//! [`context`](crate::repograph::context), [`impact`](crate::repograph::impact)
//! and [`owners`](crate::repograph::owners) each answer a different question
//! about the same target, and an assistant asking any of them almost always
//! wants at least one of the others: what is this, what breaks if I change it,
//! who do I ask about it. Three tools meant three schemas to find, three calls
//! to make, and three chances to reach for `grep` instead — and a host that
//! defers tool schemas makes *being found* the thing that decides whether a
//! tool is used at all.
//!
//! So this composes them behind one name and one target, with a [`Depth`]
//! saying how much of the answer to compute. Nothing here reads the graph
//! itself: every fact comes from the three functions above, so an answer is the
//! same whichever door it came through, and the cost of a depth is exactly the
//! cost of the calls it makes.
//!
//! # Depth
//!
//! - [`Depth::Context`] — the definition, its callers and callees, what imports
//!   it and what it changes with. The default, and the cheapest.
//! - [`Depth::Impact`] — that, plus the blast radius of the *file* the target
//!   is defined in.
//! - [`Depth::History`] — that, plus who owns the file and what changes with it.
//! - [`Depth::All`] — all three.
//!
//! Each of the last three is the context answer *and* its own addition, because
//! a blast radius without the definition it belongs to is a list of paths.

use crate::db::GraphDb;
use crate::repograph::context::{context_with, ContextOptions, ContextReport};
use crate::repograph::impact::{impact, ImpactOptions, ImpactReport};
use crate::repograph::owners::{owners, OwnersReport};
use crate::repograph::render::sanitize;
use core_storage::fs::Fs;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

/// How much of the answer one `explore` call computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Depth {
    Context,
    Impact,
    History,
    All,
}

impl Depth {
    /// The depth a caller named, or `None` when it named none of the four.
    ///
    /// Exact, lower-case names only: the MCP schema and the CLI both offer the
    /// four as an enumeration, so anything else is a caller that guessed, and
    /// silently serving the wrong depth would be worse than saying so.
    #[must_use]
    pub fn parse(s: &str) -> Option<Depth> {
        match s {
            "context" => Some(Depth::Context),
            "impact" => Some(Depth::Impact),
            "history" => Some(Depth::History),
            "all" => Some(Depth::All),
            _ => None,
        }
    }

    /// The four names, in the order they are offered, for an error message and
    /// for the schemas that enumerate them.
    pub const NAMES: [&'static str; 4] = ["context", "impact", "history", "all"];

    fn wants_impact(self) -> bool {
        matches!(self, Depth::Impact | Depth::All)
    }

    fn wants_history(self) -> bool {
        matches!(self, Depth::History | Depth::All)
    }
}

/// One target, from as many sides as the [`Depth`] asked for.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExploreReport {
    /// The target as the caller wrote it, sanitized.
    pub target: String,
    pub depth: Depth,
    /// Always computed: every depth includes the context answer.
    pub context: ContextReport,
    /// What changing the target's *file* reaches. [`Depth::Impact`] and
    /// [`Depth::All`], and only when the target resolved to a file at all.
    pub impact: Option<ImpactReport>,
    /// Who has written the target's file. [`Depth::History`] and [`Depth::All`].
    pub owners: Option<OwnersReport>,
    /// `(file, co-change score)`, strongest first — the context report's own
    /// partners, copied rather than recomputed. [`Depth::History`] and
    /// [`Depth::All`].
    pub partners: Vec<(String, f64)>,
}

/// Everything `depth` asks for about `target`.
///
/// `repo` and `full` are [`context_with`]'s: the working tree the body is
/// quoted from, and whether to quote one at all. Every other answer is the
/// graph's alone.
///
/// A target that resolves to nothing — an unknown name, or an ambiguous bare
/// one — has no file behind it, so no depth adds anything: the answer is the
/// context report, which is the one that says *why* there is nothing else.
#[must_use]
pub fn explore<F: Fs>(
    db: &GraphDb<F>,
    repo: Option<&Path>,
    target: &str,
    depth: Depth,
    full: bool,
) -> ExploreReport {
    let context = context_with(db, repo, target, &ContextOptions { source: full });
    let mut report = ExploreReport {
        target: sanitize(target),
        depth,
        context,
        impact: None,
        owners: None,
        partners: Vec::new(),
    };
    // The file the target is, or the file the target's symbol is defined in.
    // Empty when nothing answered to the target.
    let file = report.context.file.clone();
    if file.is_empty() {
        return report;
    }
    if depth.wants_impact() {
        // No `modified` set: nothing here is a diff, so no partner is one the
        // caller already has open.
        report.impact = Some(impact(
            db,
            std::slice::from_ref(&file),
            &BTreeSet::new(),
            &ImpactOptions::default(),
        ));
    }
    if depth.wants_history() {
        report.owners = owners(db, &file, None);
        report.partners = report.context.partners.clone();
    }
    report
}
