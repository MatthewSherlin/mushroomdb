//! Continuous backup of a mushroomdb store to S3-compatible object storage.
//!
//! A [`BackupWorker`] runs beside the server. Every `wal_interval_ms` it ships
//! the WAL bytes fsynced since the last tick as one immutable segment,
//! `{prefix}/wal/{generation}/{start:020}-{end:020}`; when a snapshot is
//! written it uploads `snapshot.bin`, any new `wal.N.archive` and the side
//! files; and it writes manifests (`{prefix}/manifest/{commit_seq:020}.json`,
//! pointed to by `{prefix}/LATEST`) naming consistent restore points.
//!
//! The recovery point is the segment cadence, not the snapshot interval: a
//! commit is off-box durable once the segment holding it is uploaded.
//!
//! The worker never takes an engine lock and never writes to the store; a
//! failed upload is retried with back-off and reported through
//! [`BackupHandle`], never to the write path.

/// The object-store crate this one builds on, for callers that read the bucket.
pub use object_store;

pub mod manifest;
pub mod segment;
pub mod store;
pub mod worker;

pub use manifest::{FileEntry, Manifest, SegmentEntry, MANIFEST_VERSION};
pub use store::{build_store, BackupConfig, S3Location};
pub use worker::{
    BackupCounters, BackupHandle, BackupTrigger, BackupWorker, WalSource, SIDE_FILES,
};

/// Why a backup step or its configuration failed.
#[derive(Debug)]
pub enum BackupError {
    /// The environment asked for a backup that cannot be set up.
    Config(String),
    /// The object store refused or failed a request.
    Store(object_store::Error),
    /// Reading the store's files failed.
    Io(std::io::Error),
    /// A manifest in the bucket is missing or unreadable.
    Manifest(String),
    /// The store's files are between states; the step is retried.
    Inconsistent(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::Config(m) => write!(f, "backup configuration: {m}"),
            BackupError::Store(e) => write!(f, "object store: {e}"),
            BackupError::Io(e) => write!(f, "reading the store: {e}"),
            BackupError::Manifest(m) => write!(f, "manifest: {m}"),
            BackupError::Inconsistent(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<object_store::Error> for BackupError {
    fn from(e: object_store::Error) -> Self {
        BackupError::Store(e)
    }
}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        BackupError::Io(e)
    }
}
