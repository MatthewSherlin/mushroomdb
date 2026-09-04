//! Synthetic crate root used by the extraction tests.

mod net;
mod util;

use beta_core::Ledger;
use crate::util::helper;
use serde::Serialize;

/// A record kept by the crate.
pub struct Record {
    pub id: u32,
}

impl Record {
    /// Build a record.
    pub fn new(id: u32) -> Self {
        helper(id);
        Record { id }
    }

    fn touch(&self) {
        self.bump();
        helper(self.id);
    }

    fn bump(&self) {}
}

/// Things that can describe themselves.
pub trait Summary {
    /// One-line description.
    fn summary(&self) -> String;
}

/// Shapes the crate understands.
pub enum Shape {
    Dot,
    Line,
}

/// Largest accepted identifier.
pub const LIMIT: u32 = 8;

/// Alias kept for compatibility.
pub type Alias = Record;

/// Nested module.
pub mod inner {
    /// Produce a seed value.
    pub fn seed() -> u32 {
        1
    }
}

/// Run the demo.
pub fn run(ledger: &Ledger) -> u32 {
    let r = Record::new(1);
    r.touch();
    LIMIT
}
