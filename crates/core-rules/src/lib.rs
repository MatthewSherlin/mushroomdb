pub mod def;
pub mod index;
pub use def::{evaluate, NodeView, Predicate, RuleDef};
pub use index::{candidate_spec, CandidateSpec, RuleIndex, SideIndex};
