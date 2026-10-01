//! The memory surface: `remember`, `recall` and the session brief.
//!
//! All three were first implemented inside `repograph`, the code-graph module
//! 0.7 deleted. They are the memory product's core, so they moved here before
//! that module was removed.
pub mod brief;
pub mod forget;
pub mod identity;
pub mod recall;
pub mod remember;
pub mod schema;
