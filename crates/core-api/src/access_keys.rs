//! Access keys: the `keys.json` sidecar that gives each bearer token a tier.
//!
//! One key set serves every front door (HTTP and Bolt), so a tier means the
//! same thing on both. A key resolves to a [`KeyTier`]:
//!
//! - `admin`: full access, as `MUSHROOMDB_TOKEN`.
//! - `write`: may mutate; refused the operator surfaces (`/backup`, `/stats`,
//!   `/metrics`, `/watch`, `/subscribe`).
//! - `read` with a `role`: reads the role's masked view and **never writes**,
//!   whatever write scope the role carries.
//!
//! ```json
//! {"keys": [
//!   {"name": "ops",    "hash": "<64 hex>", "tier": "admin"},
//!   {"name": "app",    "hash": "<64 hex>", "tier": "write"},
//!   {"name": "tenant", "hash": "<64 hex>", "tier": "read", "role": "tenant_a"}
//! ]}
//! ```
//!
//! The file holds only hashes: `hash` is the blake3 digest of the token, in
//! hex. A presented token is hashed once and compared with every digest in
//! constant time, so a stolen file does not yield a usable token and the
//! comparison does not depend on how much of a token matched. Tokens are at
//! least [`MIN_TOKEN_BYTES`] bytes; a shorter one never resolves.
//!
//! The file is strict. An unknown tier, a `read` key with no `role`, a `role`
//! on any other tier, a malformed or repeated hash, a repeated name, a
//! plaintext `token` field, or an unknown field refuses the whole file. A
//! half-applied key file is an access decision nobody made. On Unix the file
//! must not be accessible by group or others, as `ssh` requires of a private
//! key. No error ever carries a token.
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

use serde::{Deserialize, Serialize};

/// The shortest token a key may have, in bytes. Random 32-byte tokens are what
/// `mushroomdb keys add` issues.
pub const MIN_TOKEN_BYTES: usize = 32;

/// What a key may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyTier {
    /// Full access.
    Admin,
    /// May mutate; refused the operator surfaces.
    Write,
    /// Reads under the named role's mask. Never writes.
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

fn refuse(message: impl Into<String>) -> AccessKeysError {
    AccessKeysError(message.into())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    keys: Vec<KeyEntry>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct KeyEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    hash: String,
    tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    /// Present only in a file written by hand with a plaintext token: refused.
    #[serde(default, skip_serializing)]
    token: Option<String>,
}

/// An immutable table of keys.
struct KeyTable {
    entries: Vec<(blake3::Hash, KeyTier)>,
}

impl KeyTable {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl std::fmt::Debug for KeyTable {
    // Never prints a digest.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyTable")
            .field("keys", &self.entries.len())
            .finish()
    }
}

/// The hex blake3 digest of `token`, as `keys.json` stores it. Refuses a token
/// shorter than [`MIN_TOKEN_BYTES`].
pub fn hash_token(token: &str) -> Result<String, AccessKeysError> {
    if token.len() < MIN_TOKEN_BYTES {
        return Err(refuse(format!(
            "a token must be at least {MIN_TOKEN_BYTES} bytes"
        )));
    }
    Ok(blake3::hash(token.as_bytes()).to_hex().to_string())
}

fn parse_file(text: &str) -> Result<KeyFile, AccessKeysError> {
    // The serde message can quote a value, so it is not passed on.
    serde_json::from_str(text).map_err(|e| {
        refuse(format!(
            "keys.json is not valid (line {}, column {}); expected {{\"keys\": [{{\"hash\", \"tier\", \"role\"?, \"name\"?}}]}} with no other fields",
            e.line(),
            e.column()
        ))
    })
}

fn build_table(file: &KeyFile) -> Result<KeyTable, AccessKeysError> {
    let mut entries: Vec<(blake3::Hash, KeyTier)> = Vec::with_capacity(file.keys.len());
    let mut names: Vec<&str> = Vec::new();
    for (index, entry) in file.keys.iter().enumerate() {
        let number = index + 1;
        if entry.token.is_some() {
            return Err(refuse(format!(
                "keys.json key {number} has a plaintext \"token\" field; keys.json stores only hashes. \
                 Replace it with \"hash\": the output of `printf %s \"$TOKEN\" | mushroomdb keys hash`, \
                 or issue a new key with `mushroomdb keys add`"
            )));
        }
        let hash = blake3::Hash::from_hex(&entry.hash).map_err(|_| {
            refuse(format!(
                "keys.json key {number}: \"hash\" must be 64 hex characters (the blake3 digest of the token)"
            ))
        })?;
        if entries.iter().any(|(h, _)| *h == hash) {
            return Err(refuse(format!(
                "keys.json key {number}: its hash repeats an earlier key"
            )));
        }
        if let Some(name) = entry.name.as_deref() {
            if name.is_empty() {
                return Err(refuse(format!("keys.json key {number}: empty name")));
            }
            if names.contains(&name) {
                return Err(refuse(format!(
                    "keys.json key {number}: its name repeats an earlier key"
                )));
            }
            names.push(name);
        }
        let tier = match (entry.tier.as_str(), entry.role.clone()) {
            ("admin", None) => KeyTier::Admin,
            ("write", None) => KeyTier::Write,
            ("read", Some(role)) if !role.is_empty() => KeyTier::Read(role),
            ("read", _) => {
                return Err(refuse(format!(
                    "keys.json key {number}: a read key needs a non-empty role"
                )))
            }
            ("admin" | "write", Some(_)) => {
                return Err(refuse(format!(
                    "keys.json key {number}: only a read key takes a role"
                )))
            }
            _ => {
                return Err(refuse(format!(
                    "keys.json key {number}: tier must be admin, write or read"
                )))
            }
        };
        entries.push((hash, tier));
    }
    Ok(KeyTable { entries })
}

fn parse_table(text: &str) -> Result<KeyTable, AccessKeysError> {
    build_table(&parse_file(text)?)
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
    // Never prints a digest or a token.
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
        Self::with(KeyTable::empty(), None)
    }

    /// Parses a key file's text. [`reload`](Self::reload) has no file to read.
    pub fn from_json(text: &str) -> Result<Self, AccessKeysError> {
        Ok(Self::with(parse_table(text)?, None))
    }

    /// Reads `path`. A missing file is an empty set that remembers `path`, so
    /// a file created later is picked up by [`reload`](Self::reload). An
    /// unreadable, invalid or (on Unix) group- or world-accessible file is an
    /// error.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AccessKeysError> {
        let path = path.as_ref().to_path_buf();
        let table = read_table(&path)?;
        Ok(Self::with(table, Some(path)))
    }

    /// The file this set reads, if any.
    pub fn path(&self) -> Option<&Path> {
        self.shared.path.as_deref()
    }

    /// Re-reads the file and swaps the table in one step. Returns the number
    /// of keys now in force. On error the old table stays in force.
    pub fn reload(&self) -> Result<usize, AccessKeysError> {
        let Some(path) = self.shared.path.as_deref() else {
            return Err(refuse("this key set has no file to reload"));
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

    fn resolve_in(table: &KeyTable, presented: &str) -> Option<KeyTier> {
        if presented.len() < MIN_TOKEN_BYTES {
            return None;
        }
        let digest = blake3::hash(presented.as_bytes());
        let mut found: Option<&KeyTier> = None;
        // `Hash` equality is constant-time, and every key is compared with no
        // early exit, so the time taken does not say which one matched.
        for (hash, tier) in &table.entries {
            if digest == *hash {
                found = Some(tier);
            }
        }
        found.cloned()
    }

    /// The tier of `presented`, or `None`.
    pub fn resolve(&self, presented: &str) -> Option<KeyTier> {
        Self::resolve_in(&self.current(), presented)
    }

    /// Resolves every token against one table: what a single request would see.
    #[cfg(test)]
    fn resolve_all(&self, presented: &[&str]) -> Vec<Option<KeyTier>> {
        let table = self.current();
        presented
            .iter()
            .map(|token| Self::resolve_in(&table, token))
            .collect()
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

/// Refuses a key file that group or others can read or write.
#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), AccessKeysError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| {
            refuse(format!(
                "{} could not be read: {}",
                path.display(),
                e.kind()
            ))
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        return Err(refuse(format!(
            "{} is accessible by group or others (mode {mode:04o}); \
             it must be private, run: chmod 600 {}",
            path.display(),
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<(), AccessKeysError> {
    Ok(())
}

fn read_text(path: &Path) -> Result<Option<String>, AccessKeysError> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(refuse(format!(
                "{} could not be read: {}",
                path.display(),
                e.kind()
            )))
        }
    }
    check_private(path)?;
    std::fs::read_to_string(path).map(Some).map_err(|e| {
        refuse(format!(
            "{} could not be read: {}",
            path.display(),
            e.kind()
        ))
    })
}

fn read_table(path: &Path) -> Result<KeyTable, AccessKeysError> {
    match read_text(path)? {
        Some(text) => parse_table(&text),
        None => Ok(KeyTable::empty()),
    }
}

/// Adds one key to the file at `path`, creating it private (0600) when absent.
///
/// `hash` is what [`hash_token`] returned. Refuses a repeated name or hash, a
/// bad tier or role, and an existing file that is not private or not valid.
/// The write is atomic (a private temporary file, then a rename), so a reader
/// sees the old file or the new one.
pub fn add_key(
    path: &Path,
    name: &str,
    hash: &str,
    tier: &str,
    role: Option<&str>,
) -> Result<(), AccessKeysError> {
    let mut file = match read_text(path)? {
        Some(text) => parse_file(&text)?,
        None => KeyFile { keys: Vec::new() },
    };
    build_table(&file)?;
    if name.is_empty() {
        return Err(refuse("a key needs a name"));
    }
    file.keys.push(KeyEntry {
        name: Some(name.to_string()),
        hash: hash.to_string(),
        tier: tier.to_string(),
        role: role.map(str::to_string),
        token: None,
    });
    // Validates the new entry in the context of the others (tier, role, hash,
    // and a repeated name or hash).
    build_table(&file)?;
    let text = serde_json::to_string_pretty(&file)
        .map_err(|_| refuse("keys.json could not be written"))?;
    write_private(path, text.as_bytes())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), AccessKeysError> {
    use std::io::Write;
    let failed = |e: std::io::Error| {
        refuse(format!(
            "{} could not be written: {}",
            path.display(),
            e.kind()
        ))
    };
    let staged = path.with_extension("json.new");
    let _ = std::fs::remove_file(&staged);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(&staged).map_err(failed)?;
    out.write_all(bytes).map_err(failed)?;
    out.write_all(b"\n").map_err(failed)?;
    out.sync_all().map_err(failed)?;
    drop(out);
    std::fs::rename(&staged, path).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token of the shortest allowed length, distinct per `name`.
    fn tok(name: &str) -> String {
        format!("{name}{}", "-".repeat(MIN_TOKEN_BYTES - name.len()))
    }

    fn entry(name: &str, tier: &str, role: Option<&str>) -> String {
        let role = role.map_or(String::new(), |r| format!(r#","role":"{r}""#));
        format!(
            r#"{{"name":"{name}","hash":"{}","tier":"{tier}"{role}}}"#,
            hash_token(&tok(name)).unwrap()
        )
    }

    fn sample() -> String {
        format!(
            r#"{{"keys":[{},{},{}]}}"#,
            entry("adm", "admin", None),
            entry("wr", "write", None),
            entry("rd", "read", Some("tenant_a"))
        )
    }

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
        /// Writes `text` as a private file, as the CLI would.
        fn write_keys(&self, text: &str) -> PathBuf {
            let path = self.0.join("keys.json");
            std::fs::write(&path, text).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn each_tier_resolves() {
        let keys = AccessKeys::from_json(&sample()).unwrap();
        assert_eq!(keys.resolve(&tok("adm")), Some(KeyTier::Admin));
        assert_eq!(keys.resolve(&tok("wr")), Some(KeyTier::Write));
        assert_eq!(
            keys.resolve(&tok("rd")),
            Some(KeyTier::Read("tenant_a".into()))
        );
        assert_eq!(keys.resolve(&tok("nope")), None);
        assert_eq!(keys.resolve(""), None);
        assert_eq!(keys.resolve(&tok("adm")[..MIN_TOKEN_BYTES - 1]), None);
    }

    #[test]
    fn a_token_shorter_than_the_minimum_is_refused_everywhere() {
        assert!(hash_token("short").is_err());
        assert!(hash_token(&"x".repeat(MIN_TOKEN_BYTES - 1)).is_err());
        assert!(hash_token(&"x".repeat(MIN_TOKEN_BYTES)).is_ok());
        // A hash made by hand for a short token still never resolves.
        let short = "short";
        let hash = blake3::hash(short.as_bytes()).to_hex().to_string();
        let keys = AccessKeys::from_json(&format!(
            r#"{{"keys":[{{"hash":"{hash}","tier":"admin"}}]}}"#
        ))
        .unwrap();
        assert_eq!(keys.resolve(short), None);
    }

    #[test]
    fn a_plaintext_token_field_is_refused_with_the_conversion_and_never_echoed() {
        let err = AccessKeys::from_json(
            r#"{"keys":[{"token":"SECRET-PLAINTEXT-TOKEN-0123456789abcdef","tier":"admin"}]}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("plaintext"), "{err}");
        assert!(
            err.contains("keys hash") && err.contains("keys add"),
            "{err}"
        );
        assert!(!err.contains("SECRET"), "{err}");
    }

    #[test]
    fn a_bad_file_is_refused_whole_and_names_no_token() {
        let h = hash_token(&tok("x")).unwrap();
        for bad in [
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"root"}}]}}"#),
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"read"}}]}}"#),
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"read","role":""}}]}}"#),
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"write","role":"r"}}]}}"#),
            r#"{"keys":[{"hash":"","tier":"write"}]}"#.to_string(),
            r#"{"keys":[{"hash":"zz","tier":"write"}]}"#.to_string(),
            format!(
                r#"{{"keys":[{{"hash":"{h}","tier":"write"}},{{"hash":"{h}","tier":"admin"}}]}}"#
            ),
            format!(
                r#"{{"keys":[{{"name":"a","hash":"{h}","tier":"write"}},{{"name":"a","hash":"{}","tier":"admin"}}]}}"#,
                hash_token(&tok("y")).unwrap()
            ),
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"write","extra":1}}]}}"#),
            format!(r#"{{"keys":[{{"hash":"{h}","tier":"write"}}"#),
            r#"{"keys":[{"token":5,"tier":"write"}]}"#.to_string(),
        ] {
            let err = AccessKeys::from_json(&bad).unwrap_err().to_string();
            assert!(!err.contains(&h), "{err}");
        }
    }

    #[test]
    fn debug_never_prints_a_digest() {
        let keys = AccessKeys::from_json(&sample()).unwrap();
        let shown = format!("{keys:?}");
        assert!(
            !shown.contains(&hash_token(&tok("adm")).unwrap()),
            "{shown}"
        );
    }

    #[test]
    fn a_missing_file_is_empty_and_a_later_file_is_picked_up() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("keys.json");
        let keys = AccessKeys::load(&path).unwrap();
        assert!(keys.is_empty());
        assert!(!keys.enforcing());
        dir.write_keys(&sample());
        assert_eq!(keys.reload().unwrap(), 3);
        assert_eq!(keys.resolve(&tok("wr")), Some(KeyTier::Write));
        assert!(keys.enforcing());
    }

    #[test]
    fn a_failed_reload_keeps_the_old_table() {
        let dir = TempDir::new("failed");
        let path = dir.write_keys(&sample());
        let keys = AccessKeys::load(&path).unwrap();
        dir.write_keys("{not json");
        assert!(keys.reload().is_err());
        assert_eq!(keys.resolve(&tok("wr")), Some(KeyTier::Write));
    }

    #[test]
    fn emptying_the_file_revokes_every_key_and_still_enforces() {
        let dir = TempDir::new("empty");
        let path = dir.write_keys(&sample());
        let keys = AccessKeys::load(&path).unwrap();
        dir.write_keys(r#"{"keys":[]}"#);
        assert_eq!(keys.reload().unwrap(), 0);
        assert_eq!(keys.resolve(&tok("adm")), None);
        assert!(keys.enforcing());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(keys.reload().unwrap(), 0);
        assert!(keys.enforcing());
    }

    /// A reload swaps the whole table: one request's view is the old table or
    /// the new one, never a mix.
    #[test]
    fn a_reload_during_resolves_never_shows_a_torn_table() {
        let dir = TempDir::new("torn");
        let old = format!(
            r#"{{"keys":[{},{}]}}"#,
            entry("a", "admin", None),
            entry("b", "admin", None)
        );
        let new = format!(
            r#"{{"keys":[{},{}]}}"#,
            entry("a", "write", None),
            entry("c", "write", None)
        );
        let path = dir.write_keys(&old);
        let keys = AccessKeys::load(&path).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (a, b, c) = (tok("a"), tok("b"), tok("c"));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let keys = keys.clone();
                let stop = Arc::clone(&stop);
                let (a, b, c) = (a.clone(), b.clone(), c.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let seen = keys.resolve_all(&[&a, &b, &c]);
                        let old_table = vec![Some(KeyTier::Admin), Some(KeyTier::Admin), None];
                        let new_table = vec![Some(KeyTier::Write), None, Some(KeyTier::Write)];
                        assert!(seen == old_table || seen == new_table, "torn: {seen:?}");
                    }
                })
            })
            .collect();
        for round in 0..200 {
            let staged = dir.path().join("staged.json");
            std::fs::write(&staged, if round % 2 == 0 { &new } else { &old }).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
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
        dir.write_keys(&sample());
        keys.reload().unwrap();
        assert_eq!(clone.resolve(&tok("adm")), Some(KeyTier::Admin));
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn a_group_or_world_accessible_file_is_refused_at_load_with_its_path_and_mode() {
        if running_as_root() {
            return;
        }
        let dir = TempDir::new("mode-load");
        let path = dir.write_keys(&sample());
        for mode in [0o640, 0o604, 0o644, 0o660, 0o666, 0o602] {
            set_mode(&path, mode);
            let err = AccessKeys::load(&path).unwrap_err().to_string();
            assert!(err.contains(&path.display().to_string()), "{err}");
            assert!(err.contains(&format!("{mode:04o}")), "{err}");
            assert!(err.contains("chmod 600"), "{err}");
        }
        for mode in [0o600, 0o400] {
            set_mode(&path, mode);
            assert!(AccessKeys::load(&path).is_ok(), "{mode:04o}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_reload_of_a_loosened_file_keeps_the_old_keys() {
        if running_as_root() {
            return;
        }
        let dir = TempDir::new("mode-reload");
        let path = dir.write_keys(&sample());
        let keys = AccessKeys::load(&path).unwrap();
        set_mode(&path, 0o644);
        let err = keys.reload().unwrap_err().to_string();
        assert!(err.contains("0644"), "{err}");
        assert_eq!(keys.resolve(&tok("adm")), Some(KeyTier::Admin));
    }

    #[test]
    fn add_key_creates_a_private_file_holding_only_the_hash() {
        let dir = TempDir::new("add");
        let path = dir.path().join("keys.json");
        let token = tok("issued");
        add_key(&path, "app", &hash_token(&token).unwrap(), "write", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&token), "the file must hold no token");
        assert!(text.contains(&hash_token(&token).unwrap()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let keys = AccessKeys::load(&path).unwrap();
        assert_eq!(keys.resolve(&token), Some(KeyTier::Write));

        let second = tok("second");
        add_key(
            &path,
            "tenant",
            &hash_token(&second).unwrap(),
            "read",
            Some("tenant_a"),
        )
        .unwrap();
        assert_eq!(keys.reload().unwrap(), 2);
        assert_eq!(keys.resolve(&token), Some(KeyTier::Write));
        assert_eq!(
            keys.resolve(&second),
            Some(KeyTier::Read("tenant_a".into()))
        );
    }

    #[test]
    fn add_key_refuses_what_a_load_would_refuse_and_changes_nothing() {
        let dir = TempDir::new("add-bad");
        let path = dir.path().join("keys.json");
        let h = hash_token(&tok("one")).unwrap();
        add_key(&path, "one", &h, "admin", None).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let h2 = hash_token(&tok("two")).unwrap();
        for (name, hash, tier, role) in [
            ("one", h2.as_str(), "admin", None),
            ("two", h.as_str(), "admin", None),
            ("two", h2.as_str(), "read", None),
            ("two", h2.as_str(), "write", Some("r")),
            ("two", h2.as_str(), "root", None),
            ("two", "nothex", "write", None),
            ("", h2.as_str(), "write", None),
        ] {
            assert!(
                add_key(&path, name, hash, tier, role).is_err(),
                "{name} {tier}"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn keys_json_is_not_part_of_a_backup() {
        let dir = TempDir::new("backup");
        let store = dir.path().join("store");
        let dest = dir.path().join("dest");
        {
            let mut db = crate::GraphDb::open(&store).unwrap();
            db.insert_node("Person", "a", vec![]).unwrap();
            std::fs::write(store.join("keys.json"), sample()).unwrap();
            db.backup_to(&dest).unwrap();
        }
        assert!(dest.join("snapshot.bin").exists() || dest.join("wal.bin").exists());
        assert!(!dest.join("keys.json").exists());
    }
}
