//! Reading a code graph back as answers.
//!
//! `ingest-git` writes a repository into the graph; this module reads it out
//! again, in the shapes a person or an assistant asks for. Each tool computes
//! a plain data structure and a renderer turns it into a short digest, so the
//! same answer serves a CLI, an HTTP response and an MCP tool without being
//! computed three ways.
//!
//! Two rules hold across every tool here. The output is **deterministic**:
//! collections are sorted, ties break on the key, floats print at a fixed
//! precision, and "now" is read from the graph rather than from a clock. And
//! every string that came out of the graph passes through
//! [`sanitize`](render::sanitize) before it reaches a rendered line, so
//! repository content cannot forge a header or a line break in an assistant's
//! context.

mod map;
pub mod render;

pub use map::{repo_map, MapCommunity, MapOptions, RepoMap, SyncInfo};
pub use render::{render_map, sanitize, MAX_MAP_LINES};
