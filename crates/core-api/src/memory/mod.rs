//! The memory surface, independent of `repograph`.
//!
//! `remember`, `recall` and the session brief were implemented inside
//! `repograph` — the code-graph module 0.7 deletes. They are the memory
//! product's core, so they move here first and the old module is removed
//! afterwards.
pub mod recall;
