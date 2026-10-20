//! `RESET` and `GOODBYE`.
//!
//! `RESET` returns the connection to `READY`: it rolls back an open
//! transaction, closes an open cursor and clears a failure. It does not
//! disconnect. `GOODBYE` ends the connection and has no reply.

use super::ServerMessage;

/// The reply to a `RESET` that took effect.
pub fn reset_success() -> ServerMessage {
    ServerMessage::Success(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_success_carries_no_metadata() {
        assert_eq!(reset_success(), ServerMessage::Success(vec![]));
    }
}
