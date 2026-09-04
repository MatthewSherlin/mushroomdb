//! Reading a code graph back as answers.
//!
//! `ingest-git` writes a repository into the graph; this module reads it out
//! again, in the shapes a person or an assistant asks for. Each tool computes
//! a plain data structure and a renderer turns it into a short digest, so the
//! same answer serves a CLI, an HTTP response and an MCP tool without being
//! computed three ways.
//!
//! | Tool | Answers |
//! |---|---|
//! | [`repo_map`] | what is this repository, in one screen |
//! | [`context`] | everything known about one file or symbol |
//! | [`impact`] | what else the files in a diff reach |
//! | [`owners`] | who has written a file, and when |
//! | [`why`] | what links two things, with the evidence |
//! | [`recall::recall_digest`] | which nodes a topic is closest to |
//! | [`remember::remember`] | write a note the graph can later recall |
//! | [`stale_concepts`] | which learned concepts have drifted from their sources |
//!
//! Two rules hold across every tool here. The output is **deterministic** for
//! the same store state and the same caller-supplied "now": collections are
//! sorted, ties break on the key, floats print at a fixed precision, and every
//! answer is decided by the graph. The one value that is not is how long ago
//! the store was synced, which is measured against the system clock unless the
//! caller pins the time — see [`MapOptions::now_ts`]. And every string that
//! came out of the graph passes through [`sanitize`](render::sanitize) before
//! it reaches a rendered line, so repository content cannot forge a header or a
//! line break in an assistant's context.
//!
//! [`context`] is the one tool that reads anything outside the graph: the
//! source it quotes comes from the working tree, so what it shows is what is on
//! disk now.

mod concepts;
mod context;
mod facts;
mod impact;
mod map;
mod owners;
mod path;
pub mod recall;
pub mod remember;
pub mod render;
mod why;

pub use concepts::stale_concepts;
pub use context::{context, ContextReport, Target, MAX_SOURCE_LINES};
pub use impact::{impact, FileImpact, ImpactOptions, ImpactReport, Partner};
pub use map::{repo_map, MapCommunity, MapOptions, RepoMap, SyncInfo};
pub use owners::{owners, OwnersReport, QUARTERS};
pub use path::{shortest_path, MAX_HOPS, PATH_EDGES};
pub use recall::{
    recall_digest, MAX_EDGES_PER_HIT, MAX_EDGE_CANDIDATES, MAX_HITS, MAX_OUTPUT_BYTES,
};
pub use remember::{remember, RememberInput, NOTE_KINDS};
pub use render::{
    render_context, render_impact, render_map, render_owners, render_why, sanitize,
    MAX_CONTEXT_LINES, MAX_MAP_LINES, MAX_TOOL_LINES,
};
pub use why::{why, WhyLink, WhyReport};
