//! Synthetic nested module used by the import-resolution tests.

use crate::util::helper;
use std::io::Read;
use super::listen;

/// Open a connection.
pub fn connect(addr: &str) -> bool {
    helper(1);
    listen(addr)
}
