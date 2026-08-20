pub mod oracle;
pub mod sim_fs;
pub use oracle::Oracle;
pub use sim_fs::SimFs;

/// Recall floors for approximate vector rules.
/// These are binding constants — do not lower without explicit sign-off.
/// `QUIESCED`: fully-quiesced state (all nodes indexed, IVF fitted).
/// `RECOVERY`: any crash-recovery state (partial WAL replay, early IVF state).
pub const APPROX_RECALL_FLOOR_QUIESCED: f64 = 0.90;
pub const APPROX_RECALL_FLOOR_RECOVERY: f64 = 0.85;
