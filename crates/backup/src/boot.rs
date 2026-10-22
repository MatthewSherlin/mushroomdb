//! Restore on boot: a server whose data directory holds no store restores
//! the newest restore point of a backup prefix into it before it opens the
//! store (`MUSHROOMDB_RESTORE_FROM_S3`).
//!
//! In order:
//!
//! 1. A restore on boot that a crash stopped while it moved its files in is
//!    undone ([`crate::restore::undo_interrupted_install`]).
//! 2. A data directory that holds a store is never restored over:
//!    [`BootRestore::Skipped`], and nothing else happens.
//! 3. The prefix's bucket lease is taken. A fresh lease of another instance
//!    means that instance may still be writing: nothing is restored and
//!    nothing is written to the data directory ([`BootError::ForeignLease`]).
//! 4. The newest restore point is restored in place
//!    ([`crate::restore::restore_in_place`]) while the lease is renewed. The
//!    store gets a new lineage and records the restore point it came from,
//!    so its backup can continue under the same prefix beside the parent's
//!    chain without ever touching it (see [`crate::worker`], *Lineage*).
//!    A prefix with no backup at all is [`BootRestore::NoBackup`]: a first
//!    deploy starts an empty store. Any other failure is an error, and the
//!    data directory is left without a store.
//!
//! The lease stays held, and renewed, until the caller hands it to the
//! backup worker ([`BootLease::into_lease`], then
//! [`crate::BackupWorker::adopt_lease`]) or releases it.

use crate::lease::{new_instance_id, BucketLease, LeaseError, LeaseWatch};
use crate::restore::{
    restore_in_place, undo_interrupted_install, RestoreS3Error, RestoreS3Outcome,
};
use crate::store::BackupConfig;
use object_store::ObjectStore;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// The bucket lease a restore on boot took, renewed until it is handed on.
#[derive(Debug)]
pub struct BootLease {
    lease: Arc<BucketLease>,
    renewer: JoinHandle<()>,
}

impl BootLease {
    fn hold(lease: BucketLease, op_timeout: Duration) -> Self {
        let lease = Arc::new(lease);
        let renewer = tokio::spawn(renew(Arc::clone(&lease), op_timeout));
        Self { lease, renewer }
    }

    /// The instance id the lease names.
    pub fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }

    /// Stop renewing and hand the lease on, for
    /// [`crate::BackupWorker::adopt_lease`].
    pub async fn into_lease(self) -> Arc<BucketLease> {
        self.renewer.abort();
        let _ = self.renewer.await;
        self.lease
    }

    /// Stop renewing and delete the lease.
    pub async fn release(self, op_timeout: Duration) {
        let lease = self.into_lease().await;
        let Ok(lease) = Arc::try_unwrap(lease) else {
            return;
        };
        match tokio::time::timeout(op_timeout, lease.release()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not release the backup bucket lease; it expires by itself")
            }
            Err(_) => tracing::warn!("releasing the backup bucket lease timed out"),
        }
    }
}

async fn renew(lease: Arc<BucketLease>, op_timeout: Duration) {
    let every = lease.renew_interval().max(Duration::from_millis(1));
    loop {
        tokio::time::sleep(every).await;
        match tokio::time::timeout(op_timeout, lease.renew()).await {
            Ok(Ok(())) => {}
            Ok(Err(e @ LeaseError::Lost { .. })) => {
                tracing::warn!(error = %e, "lost the backup bucket lease during the restore");
                return;
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "bucket lease renewal failed; retrying"),
            Err(_) => tracing::warn!("bucket lease renewal timed out; retrying"),
        }
    }
}

/// What a restore on boot did.
#[derive(Debug)]
pub enum BootRestore {
    /// The data directory holds a store: nothing was restored, and no lease
    /// was taken.
    Skipped,
    /// The prefix holds no backup: the store starts empty, and its first
    /// backup starts the prefix.
    NoBackup { lease: BootLease },
    /// The newest restore point is in place.
    Restored {
        outcome: Box<RestoreS3Outcome>,
        lease: BootLease,
        /// Files an interrupted earlier restore on boot had moved in, removed
        /// before this one.
        undone: Vec<String>,
    },
}

/// Why a restore on boot failed. The data directory then holds no store.
#[derive(Debug)]
pub enum BootError {
    /// Another instance holds a fresh lease on the prefix.
    ForeignLease { holder: String },
    /// The lease could not be taken for another reason.
    Lease(String),
    /// The restore failed.
    Restore(RestoreS3Error),
    /// The data directory could not be read or cleaned up.
    Io(std::io::Error),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::ForeignLease { holder } => write!(
                f,
                "the backup bucket's lease is held by another instance ({holder}), which may \
                 still be writing to the backup; nothing was restored. Stop that instance, or \
                 wait for its lease to expire (MUSHROOMDB_BACKUP_LEASE_TTL), and start again"
            ),
            BootError::Lease(m) => write!(f, "taking the bucket lease: {m}"),
            BootError::Restore(e) => write!(f, "{e}"),
            BootError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootError {}

/// Restore the newest restore point of `config.s3_url`'s prefix into
/// `data_dir` if it holds no store. See the [module docs](self).
///
/// The lease is taken with `config`'s TTL, conditional mode and request
/// deadline.
pub async fn restore_on_boot(
    config: &BackupConfig,
    store: Arc<dyn ObjectStore>,
    data_dir: &Path,
) -> Result<BootRestore, BootError> {
    let prefix = config
        .location()
        .map_err(|e| BootError::Restore(RestoreS3Error::Config(e.to_string())))?
        .prefix;
    let dir = data_dir.to_path_buf();
    let undone = tokio::task::spawn_blocking(move || {
        let undone = undo_interrupted_install(&dir)?;
        Ok::<_, std::io::Error>((undone, mushroomdb::restore::holds_a_store(&dir)))
    })
    .await
    .map_err(|e| BootError::Io(std::io::Error::other(e.to_string())))?
    .map_err(BootError::Io)?;
    let (undone, holds) = undone;
    if !undone.is_empty() {
        tracing::warn!(
            files = undone.len(),
            "removed the files of a restore on boot that a crash interrupted"
        );
    }
    if holds {
        return Ok(BootRestore::Skipped);
    }

    let op_timeout = Duration::from_millis(config.op_timeout_ms);
    let (instance_id, mut watch) = (new_instance_id(), LeaseWatch::default());
    let acquire = BucketLease::acquire_as(
        Arc::clone(&store),
        &prefix,
        config.lease_ttl_s,
        &instance_id,
        &mut watch,
    );
    let lease = match tokio::time::timeout(op_timeout * 4, acquire).await {
        Ok(Ok(lease)) => BootLease::hold(lease, op_timeout),
        Ok(Err(LeaseError::ForeignActive { instance_id })) => {
            return Err(BootError::ForeignLease {
                holder: instance_id,
            })
        }
        Ok(Err(e @ LeaseError::Store(_))) if config.lease_conditional => {
            return Err(BootError::Lease(format!(
                "{e} (if this service rejects conditional PUT headers, set \
                 MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off)"
            )))
        }
        Ok(Err(e)) => return Err(BootError::Lease(e.to_string())),
        Err(_) => {
            return Err(BootError::Lease(
                "the bucket did not answer within the request deadline".into(),
            ))
        }
    };

    match restore_in_place(store, &prefix, data_dir).await {
        Ok(outcome) => Ok(BootRestore::Restored {
            outcome: Box::new(outcome),
            lease,
            undone,
        }),
        Err(RestoreS3Error::NoBackup { .. }) => Ok(BootRestore::NoBackup { lease }),
        Err(e) => {
            // Nothing will back this server up: let a successor in at once.
            lease.release(op_timeout).await;
            Err(BootError::Restore(e))
        }
    }
}
