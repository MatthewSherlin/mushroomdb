//! The bucket lease: an advisory, remote single-writer lock on a backup
//! prefix.
//!
//! One object, `{prefix}/LEASE`, names the instance that uploads to the
//! prefix:
//!
//! ```json
//! { "instance_id": "…", "acquired_at": "…", "renewed_at": "…", "ttl_s": 30 }
//! ```
//!
//! The holder renews it every `ttl_s / 3` and deletes it on shutdown. Another
//! instance that finds a lease renewed less than its `ttl_s` ago (by the
//! reader's clock) uploads nothing. A lease older than that — its holder died
//! or lost the network — may be taken over.
//!
//! # Advisory, not a distributed mutex
//!
//! This catches the common operator error: two machines configured with one
//! backup bucket. It does not make two writers safe. What it does and does
//! not promise:
//!
//! - **Acquisition and renewal are compare-and-swap where the service allows
//!   it.** Taking the lease is one conditional PUT — `If-None-Match: *` on an
//!   empty key, `If-Match: <etag>` over a stale lease — and so is every
//!   renewal. Two instances that start together get exactly one lease between
//!   them, and a holder whose lease was taken over learns so at its next
//!   renewal ([`LeaseError::Lost`]). Where the client cannot send conditional
//!   PUT (`MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off`), each step is a read
//!   followed by an unconditional write ([`Exclusion::BestEffort`], logged at
//!   WARN): two instances that start together may both pass the check, and
//!   the later writer wins at the next renewal.
//! - **A request whose answer was lost is not a lost lease.** An instance
//!   keeps one id across attempts ([`BucketLease::acquire_as`]). A renewal
//!   or acquire that timed out on the client but landed on the service
//!   leaves a lease naming that id; the next attempt recognises it as its
//!   own instead of waiting out a TTL behind itself.
//! - **A holder fences itself.** It may start an upload only until two thirds
//!   of a TTL after its last successful renewal ([`BucketLease::may_write`],
//!   measured on both the monotonic and the wall clock), and the worker
//!   cancels any upload still in flight at that moment. Nobody else may take
//!   the lease before a full TTL has passed, so a holder that cannot renew —
//!   partitioned from the bucket, or stalled — stops writing a third of a TTL
//!   before its lease can be taken, and writes nothing after a takeover, the
//!   partition healed or not.
//! - **Clock skew** up to that third of a TTL (10 s at the default 30 s) is
//!   absorbed: a contender whose clock runs ahead of the holder's sees the
//!   lease expire that much early, still after the holder stopped. A lease
//!   stamped in the future by a holder whose clock runs ahead counts as fresh;
//!   such a holder that dies delays a takeover by its skew.
//! - **The bucket fences too, where it can.** The worker writes every
//!   immutable object with `If-None-Match: *` and `LATEST` and the manifests
//!   with `If-Match` against the version it last wrote or read, so on a
//!   conditional service a stale holder cannot overwrite an object, and its
//!   late `LATEST` is refused once a successor has written its own.
//! - **What no lease can stop.** A stale holder's request that the service
//!   has already received completes, and a process paused between its fence
//!   check and its request (a VM frozen mid-call) sends it when it wakes. On a
//!   conditional service such a write can only add an object no manifest of
//!   the successor names (an orphan) or a manifest `LATEST` does not point
//!   at; retention deletes nothing a lineage's kept manifests name. Without
//!   conditional writes, two overlapping writers can overwrite each other's
//!   objects and corrupt a restore point.
//!
//! # Which services give real exclusion
//!
//! | Service | Acquire | Renew | Exclusion |
//! |---|---|---|---|
//! | AWS S3 | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | Cloudflare R2 | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | MinIO | `If-None-Match` / `If-Match` | `If-Match` | conditional |
//! | A service that ignores either header | refused at acquire ([`LeaseError::PreconditionsIgnored`]) | — | none: backup does not run |
//! | `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off` | read, then write | read, then write | best-effort |
//!
//! A service that accepts a conditional PUT but ignores its precondition is
//! caught by a probe at every acquire: the lease is written once more with
//! `If-None-Match: *` and once with `If-Match` on a made-up ETag, and both
//! must be refused. If either is accepted the acquire fails closed, naming
//! the cause and the switch that runs best-effort instead. Exclusion is never
//! weakened silently. The lease reports its kind through
//! [`BucketLease::exclusion`], and the worker logs it.
//!
//! # A lease stamped far in the future
//!
//! A record renewed more than its own TTL in the future, or with a TTL over
//! [`MAX_SANE_TTL_S`], is suspect: a holder with a badly wrong clock, or a
//! hand-written object. It would otherwise block the prefix for as long as
//! the clock is wrong. A contender that sees the same suspect record
//! unchanged for one of its own TTLs ([`LeaseWatch`]) takes it over, and logs
//! that it did.

use crate::manifest::object_key;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutMode, PutPayload, PutResult, UpdateVersion};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::time::Instant;

/// How many times [`BucketLease::acquire`] looks again after losing a race
/// for the key before it reports the winner.
const ACQUIRE_RACES: usize = 3;

/// The longest TTL a lease record is trusted with (one hour). A record with a
/// longer one is suspect; see the [module docs](self).
pub const MAX_SANE_TTL_S: u64 = 3600;

/// An ETag the probe sends with `If-Match`; no service ever issues it.
const PROBE_ETAG: &str = "\"mushroomdb-lease-probe-not-an-etag\"";

/// A new random instance id: 16 bytes as 32 hex digits.
pub fn new_instance_id() -> String {
    rand::random::<[u8; 16]>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What a contender has seen of a suspect foreign lease across attempts:
/// which version, and since when, on its own clock.
#[derive(Debug, Default)]
pub struct LeaseWatch {
    seen: Option<(String, Instant)>,
}

/// `{prefix}/LEASE`.
pub fn lease_key(prefix: &str) -> String {
    object_key(prefix, "LEASE")
}

/// The `LEASE` object's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// 32 hex digits, random per process start.
    pub instance_id: String,
    /// RFC 3339, the holder's clock, when it took the lease.
    pub acquired_at: String,
    /// RFC 3339, the holder's clock, when it last renewed.
    pub renewed_at: String,
    /// How long after `renewed_at` the lease stays the holder's. The holder's
    /// value governs, whatever a contender's own TTL is.
    pub ttl_s: u64,
}

impl LeaseRecord {
    /// Whether the lease is still its holder's at `now`. A `renewed_at` in
    /// the future (the holder's clock runs ahead) is fresh; one that does not
    /// parse is not.
    pub fn is_fresh(&self, now: OffsetDateTime) -> bool {
        let Ok(renewed) = OffsetDateTime::parse(&self.renewed_at, &Rfc3339) else {
            return false;
        };
        let ttl = time::Duration::seconds(i64::try_from(self.ttl_s).unwrap_or(i64::MAX));
        now - renewed < ttl
    }

    /// Whether the record cannot be trusted to expire on time: renewed more
    /// than its own TTL in the future, or with a TTL over [`MAX_SANE_TTL_S`].
    pub fn is_suspect(&self, now: OffsetDateTime) -> bool {
        if self.ttl_s > MAX_SANE_TTL_S {
            return true;
        }
        let Ok(renewed) = OffsetDateTime::parse(&self.renewed_at, &Rfc3339) else {
            return false;
        };
        renewed - now > time::Duration::seconds(self.ttl_s as i64)
    }
}

/// How strongly the lease excludes a second writer on this service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// Every acquire and renewal is one compare-and-swap on the service.
    Conditional,
    /// The service has no usable conditional PUT: each step reads, then
    /// writes, and two writers can overlap until the next renewal.
    BestEffort,
}

/// Why a lease could not be taken, renewed or released.
#[derive(Debug)]
pub enum LeaseError {
    /// Another instance holds a fresh lease on the prefix.
    ForeignActive { instance_id: String },
    /// This lease is no longer ours: the object was replaced or deleted.
    /// `instance_id` names whoever holds it now, when anyone does.
    Lost { instance_id: Option<String> },
    /// The service accepted a conditional PUT whose precondition was false:
    /// it ignores `If-None-Match` or `If-Match`, so the lease cannot exclude
    /// anyone. `header` names which.
    PreconditionsIgnored { header: &'static str },
    /// The object store refused or failed a request.
    Store(object_store::Error),
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::ForeignActive { instance_id } => write!(
                f,
                "the backup bucket's lease is held by another instance ({instance_id})"
            ),
            LeaseError::Lost {
                instance_id: Some(id),
            } => write!(f, "the bucket lease was taken over by {id}"),
            LeaseError::Lost { instance_id: None } => write!(f, "the bucket lease was lost"),
            LeaseError::PreconditionsIgnored { header } => write!(
                f,
                "the backup service ignores {header} on PUT, so the bucket lease cannot keep a \
                 second writer out; backup is not started. Set \
                 MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off to run with a best-effort lease"
            ),
            LeaseError::Store(e) => {
                write!(f, "object store: {}", crate::redact(&e.to_string()))
            }
        }
    }
}

impl std::error::Error for LeaseError {}

impl From<object_store::Error> for LeaseError {
    fn from(e: object_store::Error) -> Self {
        LeaseError::Store(e)
    }
}

#[derive(Debug)]
struct Held {
    /// The version our last write produced, for the next compare-and-swap.
    version: UpdateVersion,
    /// Taken just before the last successful write was sent: the lease is
    /// ours for `ttl` from here, on each clock.
    confirmed: Instant,
    confirmed_wall: SystemTime,
    exclusion: Exclusion,
    lost: bool,
}

/// A held lease on `{prefix}/LEASE`. See the [module docs](self).
#[derive(Debug)]
pub struct BucketLease {
    store: Arc<dyn ObjectStore>,
    key: ObjectPath,
    instance_id: String,
    ttl_s: u64,
    acquired_at: String,
    held: Mutex<Held>,
}

impl BucketLease {
    /// Take the lease on `prefix` for `ttl_s` seconds (0 counts as 1), under
    /// a new random instance id. See [`acquire_as`](Self::acquire_as).
    ///
    /// # Errors
    ///
    /// As [`acquire_as`](Self::acquire_as).
    pub async fn acquire(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        ttl_s: u64,
    ) -> Result<BucketLease, LeaseError> {
        let id = new_instance_id();
        Self::acquire_as(store, prefix, ttl_s, &id, &mut LeaseWatch::default()).await
    }

    /// Take the lease on `prefix` for `ttl_s` seconds (0 counts as 1) as
    /// `instance_id`.
    ///
    /// An instance passes the same id and `watch` to every attempt. A lease
    /// already naming the id — an earlier attempt whose write landed though
    /// its answer was lost — is taken back at once, and `watch` lets a suspect
    /// foreign lease be taken over once it has sat unchanged for a TTL.
    ///
    /// # Errors
    ///
    /// [`LeaseError::ForeignActive`] when another instance holds a fresh
    /// lease — or won the race for it against this call;
    /// [`LeaseError::PreconditionsIgnored`] when the service ignores a
    /// conditional PUT's precondition; [`LeaseError::Store`] when the service
    /// fails a request.
    pub async fn acquire_as(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        ttl_s: u64,
        instance_id: &str,
        watch: &mut LeaseWatch,
    ) -> Result<BucketLease, LeaseError> {
        let ttl_s = ttl_s.max(1);
        let key = ObjectPath::from(lease_key(prefix));
        for _ in 0..ACQUIRE_RACES {
            let current = read(&store, &key).await?;
            if let Some((Some(rec), version)) = &current {
                if rec.instance_id != instance_id {
                    foreign_blocks(rec, version, ttl_s, watch)?;
                }
            }
            let (confirmed, confirmed_wall) = (Instant::now(), SystemTime::now());
            let stamp = rfc3339(confirmed_wall);
            let record = LeaseRecord {
                instance_id: instance_id.to_string(),
                acquired_at: stamp.clone(),
                renewed_at: stamp.clone(),
                ttl_s,
            };
            let mode = match &current {
                None => PutMode::Create,
                Some((_, v)) => PutMode::Update(v.clone()),
            };
            let (result, exclusion) = match put(&store, &key, &record, mode).await {
                Ok(r) => (r, Exclusion::Conditional),
                Err(object_store::Error::AlreadyExists { .. })
                | Err(object_store::Error::Precondition { .. }) => continue,
                Err(object_store::Error::NotImplemented) => (
                    put(&store, &key, &record, PutMode::Overwrite).await?,
                    Exclusion::BestEffort,
                ),
                Err(e) => return Err(e.into()),
            };
            let lease = BucketLease {
                store,
                key,
                instance_id: instance_id.to_string(),
                ttl_s,
                acquired_at: stamp,
                held: Mutex::new(Held {
                    version: version_of(result),
                    confirmed,
                    confirmed_wall,
                    exclusion,
                    lost: false,
                }),
            };
            if exclusion == Exclusion::Conditional {
                if let Err(e) = lease.probe(&record).await {
                    if matches!(e, LeaseError::PreconditionsIgnored { .. }) {
                        // Hand the prefix back: this lease excludes nobody.
                        let _ = lease.store.delete(&lease.key).await;
                    }
                    return Err(e);
                }
            }
            *watch = LeaseWatch::default();
            return Ok(lease);
        }
        // Lost every race: whoever won holds it now.
        let holder = read(&store, &key)
            .await?
            .and_then(|(r, _)| r)
            .map_or_else(|| "unknown".to_string(), |r| r.instance_id);
        Err(LeaseError::ForeignActive {
            instance_id: holder,
        })
    }

    /// Check that the service refuses both preconditions: a create over the
    /// existing lease, and an update against an ETag it never issued.
    /// `NotImplemented` (the client cannot send it) leaves the lease
    /// best-effort; an accepted write means the service ignores the header.
    async fn probe(&self, record: &LeaseRecord) -> Result<(), LeaseError> {
        let fake = UpdateVersion {
            e_tag: Some(PROBE_ETAG.to_string()),
            version: None,
        };
        for (mode, header) in [
            (PutMode::Create, "If-None-Match"),
            (PutMode::Update(fake), "If-Match"),
        ] {
            match put(&self.store, &self.key, record, mode).await {
                Err(object_store::Error::AlreadyExists { .. })
                | Err(object_store::Error::Precondition { .. }) => {}
                Err(object_store::Error::NotImplemented) => {
                    self.lock().exclusion = Exclusion::BestEffort;
                }
                Ok(_) => return Err(LeaseError::PreconditionsIgnored { header }),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Give up the lease without a request: a write elsewhere showed that
    /// another writer is active.
    pub(crate) fn mark_lost(&self) {
        self.lock().lost = true;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This process's instance id, as the `LEASE` object names it.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The TTL this lease was taken with, in seconds.
    pub fn ttl_s(&self) -> u64 {
        self.ttl_s
    }

    /// How often to [`renew`](Self::renew): a third of the TTL.
    pub fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.ttl_s * 1000 / 3)
    }

    /// How strongly this lease excludes a second writer on this service.
    pub fn exclusion(&self) -> Exclusion {
        self.lock().exclusion
    }

    /// Whether the lease was found replaced or deleted. A lost lease never
    /// comes back; take a new one.
    pub fn is_lost(&self) -> bool {
        self.lock().lost
    }

    /// The moment, on Tokio's clock, after which this holder must not write:
    /// two thirds of a TTL after the last successful renewal. `None` once the
    /// lease is lost or that moment has passed on either clock.
    pub fn write_deadline(&self) -> Option<Instant> {
        let h = self.lock();
        let fence = Duration::from_millis(self.ttl_s * 2000 / 3);
        let wall_ok = match SystemTime::now().duration_since(h.confirmed_wall) {
            Ok(elapsed) => elapsed < fence,
            // The wall clock stepped back past the renewal: the monotonic
            // clock alone decides.
            Err(_) => true,
        };
        let deadline = h.confirmed + fence;
        (!h.lost && wall_ok && Instant::now() < deadline).then_some(deadline)
    }

    /// Whether this holder may start a write now. See
    /// [`write_deadline`](Self::write_deadline).
    pub fn may_write(&self) -> bool {
        self.write_deadline().is_some()
    }

    /// Extend the lease: rewrite it with a new `renewed_at`, provided it is
    /// still exactly ours.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Lost`] when the object was replaced or deleted since our
    /// last write — the lease is then lost for good; [`LeaseError::Store`]
    /// when the service fails a request (retry; the fence keeps the holder
    /// safe meanwhile).
    pub async fn renew(&self) -> Result<(), LeaseError> {
        let (exclusion, version) = {
            let h = self.lock();
            if h.lost {
                return Err(LeaseError::Lost { instance_id: None });
            }
            (h.exclusion, h.version.clone())
        };
        let (confirmed, confirmed_wall) = (Instant::now(), SystemTime::now());
        let record = LeaseRecord {
            instance_id: self.instance_id.clone(),
            acquired_at: self.acquired_at.clone(),
            renewed_at: rfc3339(confirmed_wall),
            ttl_s: self.ttl_s,
        };
        let written = match exclusion {
            Exclusion::Conditional => {
                match put(&self.store, &self.key, &record, PutMode::Update(version)).await {
                    Err(object_store::Error::NotImplemented) => {
                        self.lock().exclusion = Exclusion::BestEffort;
                        self.renew_best_effort(&record).await
                    }
                    Err(object_store::Error::Precondition { .. })
                    | Err(object_store::Error::NotFound { .. }) => self.reconcile(&record).await,
                    other => other.map_err(LeaseError::from),
                }
            }
            Exclusion::BestEffort => self.renew_best_effort(&record).await,
        }?;
        let mut h = self.lock();
        h.version = version_of(written);
        h.confirmed = confirmed;
        h.confirmed_wall = confirmed_wall;
        Ok(())
    }

    /// A compare-and-swap was refused. If the lease still names us, a write
    /// of ours landed though its answer was lost: adopt the version it left
    /// and swap again. Otherwise the lease is lost.
    async fn reconcile(&self, record: &LeaseRecord) -> Result<PutResult, LeaseError> {
        match read(&self.store, &self.key).await? {
            Some((Some(r), v)) if r.instance_id == self.instance_id => {
                match put(&self.store, &self.key, record, PutMode::Update(v)).await {
                    Ok(w) => Ok(w),
                    Err(object_store::Error::Precondition { .. })
                    | Err(object_store::Error::NotFound { .. }) => Err(self.lose().await),
                    Err(e) => Err(e.into()),
                }
            }
            _ => Err(self.lose().await),
        }
    }

    /// Read the lease and rewrite it only if it still names us.
    async fn renew_best_effort(&self, record: &LeaseRecord) -> Result<PutResult, LeaseError> {
        match read(&self.store, &self.key).await? {
            Some((Some(r), _)) if r.instance_id == self.instance_id => {
                Ok(put(&self.store, &self.key, record, PutMode::Overwrite).await?)
            }
            _ => Err(self.lose().await),
        }
    }

    /// Mark the lease lost, and say who holds it now.
    async fn lose(&self) -> LeaseError {
        self.lock().lost = true;
        let holder = read(&self.store, &self.key)
            .await
            .ok()
            .flatten()
            .and_then(|(r, _)| r)
            .map(|r| r.instance_id)
            .filter(|id| *id != self.instance_id);
        LeaseError::Lost {
            instance_id: holder,
        }
    }

    /// Give the lease up: DELETE `{prefix}/LEASE` if it still names us. A
    /// lost lease, or one another instance now holds, is left alone.
    ///
    /// The check and the delete are two requests (object_store 0.11 has no
    /// conditional DELETE): a takeover landing between them would be deleted,
    /// and its holder would rewrite it at its next renewal.
    ///
    /// # Errors
    ///
    /// [`LeaseError::Store`] when the service fails a request; the lease then
    /// expires by itself after its TTL.
    pub async fn release(self) -> Result<(), LeaseError> {
        if self.is_lost() {
            return Ok(());
        }
        match read(&self.store, &self.key).await? {
            Some((Some(r), _)) if r.instance_id == self.instance_id => {
                match self.store.delete(&self.key).await {
                    Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }
            _ => Ok(()),
        }
    }
}

/// The lease object, parsed when it parses, with the version to swap it by.
async fn read(
    store: &Arc<dyn ObjectStore>,
    key: &ObjectPath,
) -> Result<Option<(Option<LeaseRecord>, UpdateVersion)>, LeaseError> {
    let got = match store.get(key).await {
        Ok(g) => g,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let version = UpdateVersion {
        e_tag: got.meta.e_tag.clone(),
        version: got.meta.version.clone(),
    };
    let bytes = got.bytes().await?;
    Ok(Some((serde_json::from_slice(&bytes).ok(), version)))
}

async fn put(
    store: &Arc<dyn ObjectStore>,
    key: &ObjectPath,
    record: &LeaseRecord,
    mode: PutMode,
) -> Result<PutResult, object_store::Error> {
    let body = serde_json::to_vec(record).expect("a lease record always serialises");
    store
        .put_opts(key, PutPayload::from(body), mode.into())
        .await
}

/// Whether a foreign record keeps this contender out. A fresh record does,
/// unless it is suspect and has sat unchanged for one of our TTLs.
fn foreign_blocks(
    rec: &LeaseRecord,
    version: &UpdateVersion,
    ttl_s: u64,
    watch: &mut LeaseWatch,
) -> Result<(), LeaseError> {
    let now = OffsetDateTime::now_utc();
    let blocked = || LeaseError::ForeignActive {
        instance_id: rec.instance_id.clone(),
    };
    if !rec.is_fresh(now) {
        return Ok(());
    }
    if !rec.is_suspect(now) {
        watch.seen = None;
        return Err(blocked());
    }
    let tag = format!(
        "{}|{}|{}",
        version.e_tag.as_deref().unwrap_or(""),
        version.version.as_deref().unwrap_or(""),
        rec.renewed_at
    );
    match &watch.seen {
        Some((seen, since)) if *seen == tag => {
            if since.elapsed() >= Duration::from_secs(ttl_s) {
                tracing::warn!(
                    holder = %rec.instance_id,
                    renewed_at = %rec.renewed_at,
                    ttl_s = rec.ttl_s,
                    "taking over a suspect bucket lease that has not changed for a TTL"
                );
                return Ok(());
            }
        }
        _ => {
            tracing::warn!(
                holder = %rec.instance_id,
                renewed_at = %rec.renewed_at,
                ttl_s = rec.ttl_s,
                "the bucket lease is stamped far in the future or has an absurd TTL; \
                 it is taken over if it stays unchanged for a TTL"
            );
            watch.seen = Some((tag, Instant::now()));
        }
    }
    Err(blocked())
}

fn version_of(r: PutResult) -> UpdateVersion {
    UpdateVersion {
        e_tag: r.e_tag,
        version: r.version,
    }
}

fn rfc3339(t: SystemTime) -> String {
    OffsetDateTime::from(t).format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(renewed: OffsetDateTime, ttl_s: u64) -> LeaseRecord {
        let s = renewed.format(&Rfc3339).unwrap();
        LeaseRecord {
            instance_id: "x".into(),
            acquired_at: s.clone(),
            renewed_at: s,
            ttl_s,
        }
    }

    #[test]
    fn freshness_is_strictly_less_than_the_ttl() {
        let now = OffsetDateTime::now_utc();
        assert!(record(now - time::Duration::seconds(29), 30).is_fresh(now));
        assert!(!record(now - time::Duration::seconds(30), 30).is_fresh(now));
        assert!(record(now + time::Duration::seconds(5), 30).is_fresh(now));
        let mut garbage = record(now, 30);
        garbage.renewed_at = "yesterday".into();
        assert!(!garbage.is_fresh(now));
        assert!(record(now, u64::MAX).is_fresh(now));
    }

    #[test]
    fn far_future_stamps_and_absurd_ttls_are_suspect() {
        let now = OffsetDateTime::now_utc();
        assert!(!record(now, 30).is_suspect(now));
        assert!(!record(now + time::Duration::seconds(20), 30).is_suspect(now));
        assert!(record(now + time::Duration::seconds(31), 30).is_suspect(now));
        assert!(record(now, MAX_SANE_TTL_S + 1).is_suspect(now));
        assert!(!record(now, MAX_SANE_TTL_S).is_suspect(now));
    }

    /// A suspect lease blocks until it has sat unchanged for our TTL.
    #[tokio::test(start_paused = true)]
    async fn a_suspect_lease_unchanged_for_a_ttl_is_taken_over() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let ahead = OffsetDateTime::now_utc() + time::Duration::days(1);
        let mut rec = record(ahead, 30);
        rec.instance_id = "clock-a-day-ahead".into();
        store
            .put(
                &ObjectPath::from(lease_key("p")),
                PutPayload::from(serde_json::to_vec(&rec).unwrap()),
            )
            .await
            .unwrap();
        let mut watch = LeaseWatch::default();
        let me = new_instance_id();
        for _ in 0..2 {
            assert!(matches!(
                BucketLease::acquire_as(Arc::clone(&store), "p", 30, &me, &mut watch).await,
                Err(LeaseError::ForeignActive { .. })
            ));
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        tokio::time::advance(Duration::from_secs(11)).await;
        let l = BucketLease::acquire_as(Arc::clone(&store), "p", 30, &me, &mut watch)
            .await
            .expect("unchanged for 31 s of a 30 s TTL");
        assert_eq!(l.instance_id(), me);
    }

    /// An attempt whose write landed though its answer was lost is
    /// recognised by the next attempt under the same id.
    #[tokio::test]
    async fn a_lease_naming_our_id_is_taken_back_at_once() {
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let me = new_instance_id();
        let mut watch = LeaseWatch::default();
        let first = BucketLease::acquire_as(Arc::clone(&store), "p", 30, &me, &mut watch)
            .await
            .unwrap();
        drop(first); // its answer "never arrived"
        let again = BucketLease::acquire_as(Arc::clone(&store), "p", 30, &me, &mut watch)
            .await
            .expect("our own fresh lease is ours");
        again.renew().await.unwrap();
    }

    #[test]
    fn the_key_sits_at_the_prefix_root() {
        assert_eq!(lease_key("a/b"), "a/b/LEASE");
        assert_eq!(lease_key(""), "LEASE");
    }
}
