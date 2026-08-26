use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Deliberate near-duplicate of IdMap: IdMap gains node-specific behavior in
// later plans (free-list on delete) while Interner never deletes. Do not unify.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Interner {
    to_sym: HashMap<String, u32>,
    to_str: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&sym) = self.to_sym.get(s) {
            return sym;
        }
        let sym = self.to_str.len() as u32;
        self.to_sym.insert(s.to_string(), sym);
        self.to_str.push(s.to_string());
        sym
    }

    pub fn resolve(&self, sym: u32) -> Option<&str> {
        self.to_str.get(sym as usize).map(|s| s.as_str())
    }

    pub fn get(&self, s: &str) -> Option<u32> {
        self.to_sym.get(s).copied()
    }

    /// Number of interned symbols (also the id `intern` assigns next).
    pub fn len(&self) -> usize {
        self.to_str.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_str.is_empty()
    }

    /// Roll back to the first `len` symbols. Ids are assigned densely and
    /// never persisted before their WAL record, so dropping the tail is the
    /// exact inverse of the speculative `intern` calls made since `len` was
    /// captured. No-op if `len >= self.len()`.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.to_str.len() {
            return;
        }
        for s in self.to_str.drain(len..) {
            self.to_sym.remove(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_idempotent_and_resolvable() {
        let mut i = Interner::new();
        let a = i.intern("Person");
        let b = i.intern("Company");
        assert_eq!(i.intern("Person"), a);
        assert_ne!(a, b);
        assert_eq!(i.resolve(b), Some("Company"));
        assert_eq!(i.get("Person"), Some(a));
        assert_eq!(i.get("Nope"), None);
    }
}
