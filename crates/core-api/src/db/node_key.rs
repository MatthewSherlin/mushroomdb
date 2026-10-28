//! Generated keys for nodes created without a string `id` (QL-NM).
//!
//! Every node has a key. `CREATE (n:L {id: 'x'})` names it; `CREATE ()`,
//! `CREATE (:L {name: 'a'})` and `CREATE (:L {id: 1})` do not, and the engine
//! gives such a node [`GENERATED_KEY_PREFIX`] followed by 32 lowercase hex
//! digits (128 random bits).
//!
//! The key is chosen when the statement runs and travels in the `InsertNode`
//! op, so the WAL record carries it: replay, `open_at`, a snapshot, a backup
//! and an export read it back and never draw a new one. Random rather than a
//! counter, because a counter would hand a deleted node's key to a later node
//! once the store reopened, and two nodes would then share one history.
//! [`generate_node_key`] also draws again while the key is taken, so a
//! generated key never lands on a live key.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// What every generated key begins with. Reserved for generated keys: a key
/// written by hand with it is accepted (an export carries generated keys, and
/// its import must restore the same nodes), and names that node like any key.
pub const GENERATED_KEY_PREFIX: &str = "_:";

/// Whether `key` has the shape of a generated key.
pub fn is_generated_key(key: &str) -> bool {
    key.len() == GENERATED_KEY_PREFIX.len() + 32
        && key.starts_with(GENERATED_KEY_PREFIX)
        && key[GENERATED_KEY_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 64 random bits. `RandomState` seeds from the operating system's random
/// source; the counter and the clock make two calls differ even under one
/// seed.
fn random_u64() -> u64 {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(CALLS.fetch_add(1, Ordering::Relaxed));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    h.write_u128(nanos);
    h.finish()
}

/// A fresh generated key that `taken` does not hold.
pub(crate) fn generate_node_key(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let key = format!(
            "{GENERATED_KEY_PREFIX}{:016x}{:016x}",
            random_u64(),
            random_u64()
        );
        if !taken(&key) {
            return key;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_key_has_its_shape_and_does_not_repeat() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let key = generate_node_key(|_| false);
            assert!(is_generated_key(&key), "{key}");
            assert!(seen.insert(key));
        }
    }

    #[test]
    fn a_taken_key_is_drawn_again() {
        let first = generate_node_key(|_| false);
        let calls = std::cell::Cell::new(0);
        let key = generate_node_key(|_| {
            calls.set(calls.get() + 1);
            calls.get() < 3
        });
        assert_eq!(calls.get(), 3);
        assert!(is_generated_key(&key));
        assert_ne!(key, first);
    }

    #[test]
    fn only_the_exact_shape_is_generated() {
        assert!(!is_generated_key("_:abc"));
        assert!(!is_generated_key("alice"));
        assert!(!is_generated_key(&format!("_:{}", "A".repeat(32))));
        assert!(is_generated_key(&format!("_:{}", "0f".repeat(16))));
    }
}
