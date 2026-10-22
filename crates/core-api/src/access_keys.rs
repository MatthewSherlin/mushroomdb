//! Access keys: the `keys.json` sidecar that gives each bearer token a tier.
//!
//! One key set serves every front door (HTTP and Bolt), so a tier means the
//! same thing on both. A key resolves to a [`KeyTier`]:
//!
//! - `admin`: full access, as `MUSHROOMDB_TOKEN`.
//! - `write`: may mutate; refused the operator surfaces (`/backup`, `/stats`,
//!   `/metrics`, `/watch`, `/subscribe`).
//! - `read` with a `role`: read-only and visibility-masked, as a role token.
//!
//! ```json
//! {"keys": [
//!   {"token": "tok-a", "tier": "admin"},
//!   {"token": "tok-w", "tier": "write"},
//!   {"token": "tok-r", "tier": "read", "role": "tenant_a"}
//! ]}
//! ```
//!
//! The file is strict: an unknown tier, a `read` key with no `role`, a `role`
//! on any other tier, an empty or repeated token, or an unknown field refuses
//! the whole file. A half-applied key file is an access decision nobody made.
//! No error ever carries a token.
//!
//! A reload swaps the whole table at once; a request resolves against exactly
//! one table. A reload that fails keeps the table in force.
//!
//! Once any key has been loaded, the set keeps enforcing authentication even
//! if a later reload leaves it empty: removing every key must lock the server,
//! not open it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::Deserialize;

/// What a key may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTier {
    /// Full access.
    Admin,
    /// May mutate; refused the operator surfaces.
    Write,
    /// Read-only, masked by the named role.
    Read(String),
}

/// Why a key file was refused. Never contains a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessKeysError(String);

impl std::fmt::Display for AccessKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AccessKeysError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    keys: Vec<KeyEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyEntry {
    token: String,
    tier: String,
    #[serde(default)]
    role: Option<String>,
}

/// An immutable table of keys.
struct KeyTable {
    entries: Vec<(String, KeyTier)>,
}

impl std::fmt::Debug for KeyTable {
    // Never prints a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyTable")
            .field("keys", &self.entries.len())
            .finish()
    }
}

fn parse_table(text: &str) -> Result<KeyTable, AccessKeysError> {
    // The serde message can quote a value, so it is not passed on.
    let file: KeyFile = serde_json::from_str(text).map_err(|e| {
        AccessKeysError(format!(
            "keys.json is not valid (line {}, column {}); expected {{\"keys\": [{{\"token\", \"tier\", \"role\"?}}]}} with no other fields",
            e.line(),
            e.column()
        ))
    })?;
    let mut entries: Vec<(String, KeyTier)> = Vec::with_capacity(file.keys.len());
    for (index, entry) in file.keys.into_iter().enumerate() {
        let number = index + 1;
        if entry.token.is_empty() {
            return Err(AccessKeysError(format!(
                "keys.json key {number}: empty token"
            )));
        }
        if entries.iter().any(|(t, _)| t == &entry.token) {
            return Err(AccessKeysError(format!(
                "keys.json key {number}: its token repeats an earlier key"
            )));
        }
        let tier = match (entry.tier.as_str(), entry.role) {
            ("admin", None) => KeyTier::Admin,
            ("write", None) => KeyTier::Write,
            ("read", Some(role)) if !role.is_empty() => KeyTier::Read(role),
            ("read", _) => {
                return Err(AccessKeysError(format!(
                    "keys.json key {number}: a read key needs a non-empty role"
                )))
            }
            ("admin" | "write", Some(_)) => {
                return Err(AccessKeysError(format!(
                    "keys.json key {number}: only a read key takes a role"
                )))
            }
            _ => {
                return Err(AccessKeysError(format!(
                    "keys.json key {number}: tier must be admin, write or read"
                )))
            }
        };
        entries.push((entry.token, tier));
    }
    Ok(KeyTable { entries })
}

/// Byte equality that does not stop at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct Shared {
    table: RwLock<Arc<KeyTable>>,
    /// Sticky: set once a table with a key is in force. See the module docs.
    ever_populated: AtomicBool,
    path: Option<PathBuf>,
}

/// A shared, reloadable key set. Clones share one table.
#[derive(Clone)]
pub struct AccessKeys {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AccessKeys {
    // Never prints a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessKeys")
            .field("keys", &self.len())
            .finish()
    }
}

impl AccessKeys {
    fn with(table: KeyTable, path: Option<PathBuf>) -> Self {
        let populated = !table.entries.is_empty();
        Self {
            shared: Arc::new(Shared {
                table: RwLock::new(Arc::new(table)),
                ever_populated: AtomicBool::new(populated),
                path,
            }),
        }
    }

    /// No keys, no file.
    pub fn empty() -> Self {
        Self::with(
            KeyTable {
                entries: Vec::new(),
            },
            None,
        )
    }

    /// Parses a key file's text. [`reload`](Self::reload) has no file to read.
    pub fn from_json(text: &str) -> Result<Self, AccessKeysError> {
        Ok(Self::with(parse_table(text)?, None))
    }

    /// Reads `path`. A missing file is an empty set that remembers `path`, so
    /// a file created later is picked up by [`reload`](Self::reload). An
    /// unreadable or invalid file is an error.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AccessKeysError> {
        let path = path.as_ref().to_path_buf();
        let table = read_table(&path)?;
        Ok(Self::with(table, Some(path)))
    }

    /// Re-reads the file and swaps the table in one step. Returns the number
    /// of keys now in force. On error the old table stays in force.
    pub fn reload(&self) -> Result<usize, AccessKeysError> {
        let Some(path) = self.shared.path.as_deref() else {
            return Err(AccessKeysError("this key set has no file to reload".into()));
        };
        let table = read_table(path)?;
        let count = table.entries.len();
        if count > 0 {
            self.shared.ever_populated.store(true, Ordering::SeqCst);
        }
        *self
            .shared
            .table
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(table);
        Ok(count)
    }

    fn current(&self) -> Arc<KeyTable> {
        Arc::clone(
            &self
                .shared
                .table
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// The tier of `presented`, or `None`. Every key is compared with no early
    /// exit, so the time taken does not say which one matched or how far.
    pub fn resolve(&self, presented: &str) -> Option<KeyTier> {
        if presented.is_empty() {
            return None;
        }
        let table = self.current();
        let mut found: Option<&KeyTier> = None;
        for (token, tier) in &table.entries {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                found = Some(tier);
            }
        }
        found.cloned()
    }

    /// How many keys are in force.
    pub fn len(&self) -> usize {
        self.current().entries.len()
    }

    /// Whether no key is in force right now.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether authentication must be enforced on account of this set: a key
    /// is in force, or one ever was. A front door with no other token is open
    /// only while this is false.
    pub fn enforcing(&self) -> bool {
        self.shared.ever_populated.load(Ordering::SeqCst) || !self.is_empty()
    }
}

fn read_table(path: &Path) -> Result<KeyTable, AccessKeysError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_table(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(KeyTable {
            entries: Vec::new(),
        }),
        Err(e) => Err(AccessKeysError(format!(
            "keys.json could not be read: {}",
            e.kind()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"keys":[
        {"token":"tok-a","tier":"admin"},
        {"token":"tok-w","tier":"write"},
        {"token":"tok-r","tier":"read","role":"tenant_a"}]}"#;

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mushroomdb-access-keys-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn each_tier_resolves() {
        let keys = AccessKeys::from_json(SAMPLE).unwrap();
        assert_eq!(keys.resolve("tok-a"), Some(KeyTier::Admin));
        assert_eq!(keys.resolve("tok-w"), Some(KeyTier::Write));
        assert_eq!(
            keys.resolve("tok-r"),
            Some(KeyTier::Read("tenant_a".into()))
        );
        assert_eq!(keys.resolve("tok-x"), None);
        assert_eq!(keys.resolve(""), None);
        assert_eq!(keys.resolve("tok-"), None);
    }

    #[test]
    fn a_bad_file_is_refused_whole_and_names_no_token() {
        for bad in [
            r#"{"keys":[{"token":"SECRET1","tier":"root"}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"read"}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"read","role":""}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"write","role":"r"}]}"#,
            r#"{"keys":[{"token":"","tier":"write"}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"write"},{"token":"SECRET1","tier":"admin"}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"write","extra":1}]}"#,
            r#"{"keys":[{"token":"SECRET1","tier":"write"}"#,
            r#"{"keys":[{"token":5,"tier":"write"}]}"#,
        ] {
            let err = AccessKeys::from_json(bad).unwrap_err().to_string();
            assert!(!err.contains("SECRET1"), "{err}");
        }
    }

    #[test]
    fn debug_never_prints_a_token() {
        let keys = AccessKeys::from_json(SAMPLE).unwrap();
        let shown = format!("{keys:?}");
        assert!(!shown.contains("tok-"), "{shown}");
    }

    #[test]
    fn a_missing_file_is_empty_and_a_later_file_is_picked_up() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("keys.json");
        let keys = AccessKeys::load(&path).unwrap();
        assert!(keys.is_empty());
        assert!(!keys.enforcing());
        std::fs::write(&path, SAMPLE).unwrap();
        assert_eq!(keys.reload().unwrap(), 3);
        assert_eq!(keys.resolve("tok-w"), Some(KeyTier::Write));
        assert!(keys.enforcing());
    }

    #[test]
    fn a_failed_reload_keeps_the_old_table() {
        let dir = TempDir::new("failed");
        let path = dir.path().join("keys.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let keys = AccessKeys::load(&path).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(keys.reload().is_err());
        assert_eq!(keys.resolve("tok-w"), Some(KeyTier::Write));
    }

    #[test]
    fn emptying_the_file_revokes_every_key_and_still_enforces() {
        let dir = TempDir::new("empty");
        let path = dir.path().join("keys.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let keys = AccessKeys::load(&path).unwrap();
        std::fs::write(&path, r#"{"keys":[]}"#).unwrap();
        assert_eq!(keys.reload().unwrap(), 0);
        assert_eq!(keys.resolve("tok-a"), None);
        assert!(keys.enforcing());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(keys.reload().unwrap(), 0);
        assert!(keys.enforcing());
    }

    /// A reload swaps the whole table: a reader racing it sees the old table
    /// or the new one, never a key missing from both or half of each.
    #[test]
    fn a_reload_during_resolves_never_shows_a_torn_table() {
        let dir = TempDir::new("torn");
        let path = dir.path().join("keys.json");
        let old = r#"{"keys":[{"token":"a","tier":"admin"},{"token":"b","tier":"admin"}]}"#;
        let new = r#"{"keys":[{"token":"a","tier":"write"},{"token":"c","tier":"write"}]}"#;
        std::fs::write(&path, old).unwrap();
        let keys = AccessKeys::load(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let keys = keys.clone();
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        // `a` is in both tables, so it always resolves.
                        let a = keys.resolve("a");
                        assert!(matches!(a, Some(KeyTier::Admin | KeyTier::Write)), "{a:?}");
                        // `b` and `c` belong to different tables.
                        assert!(keys.len() == 2);
                    }
                })
            })
            .collect();
        for round in 0..200 {
            // The file is replaced by rename, as an operator's tooling should.
            let staged = dir.path().join("keys.json.new");
            std::fs::write(&staged, if round % 2 == 0 { new } else { old }).unwrap();
            std::fs::rename(&staged, &path).unwrap();
            assert_eq!(keys.reload().unwrap(), 2);
        }
        stop.store(true, Ordering::SeqCst);
        for reader in readers {
            reader.join().unwrap();
        }
    }

    #[test]
    fn clones_share_one_table() {
        let dir = TempDir::new("clones");
        let path = dir.path().join("keys.json");
        let keys = AccessKeys::load(&path).unwrap();
        let clone = keys.clone();
        std::fs::write(&path, SAMPLE).unwrap();
        keys.reload().unwrap();
        assert_eq!(clone.resolve("tok-a"), Some(KeyTier::Admin));
    }
}
