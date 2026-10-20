//! `HELLO` and the mapping from Bolt credentials to a mushroomdb identity.
//!
//! At Bolt 5.0 and 4.4 the client authenticates inside `HELLO` (`LOGON` starts
//! at 5.1 and is not offered). The `HELLO` extra map carries `scheme`,
//! `principal` and `credentials`:
//!
//! - `basic`: `credentials` is a mushroomdb token. `principal` is ignored,
//!   because mushroomdb has no user table: a token carries its role.
//! - `bearer`: `credentials` is a token, the same way.
//! - `none`: anonymous, accepted only when unauthenticated access was turned on
//!   explicitly.
//!
//! Every way of failing returns the same [`BoltError::Unauthorized`], so a
//! failed login does not say whether a token exists, whether its role is gone,
//! or which part of the request was wrong.

use std::collections::HashMap;
use std::sync::Arc;

use super::{PackValue, ServerMessage};
use crate::error::BoltError;

/// How the client says it authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthToken {
    /// `credentials` is the token; `principal` is kept only for logging.
    Basic {
        principal: Option<String>,
        credentials: String,
    },
    Bearer {
        credentials: String,
    },
    None,
    /// A scheme this server does not know. Kept so the refusal is the same as
    /// any other failed login.
    Unknown,
}

/// A decoded `HELLO`.
#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub user_agent: Option<String>,
    pub auth: AuthToken,
}

fn string_field<'a>(map: &'a [(String, PackValue)], key: &str) -> Option<&'a str> {
    map.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            PackValue::String(s) => Some(s.as_str()),
            _ => None,
        })
}

impl Hello {
    /// Reads the fields of a `HELLO` structure: one map.
    pub fn from_fields(fields: Vec<PackValue>) -> Result<Self, BoltError> {
        let [PackValue::Map(extra)] = fields.as_slice() else {
            return Err(BoltError::Invalid(
                "HELLO takes exactly one map field".into(),
            ));
        };
        let user_agent = string_field(extra, "user_agent").map(str::to_string);
        let credentials = string_field(extra, "credentials").map(str::to_string);
        let auth = match string_field(extra, "scheme") {
            Some("basic") => match credentials {
                Some(credentials) => AuthToken::Basic {
                    principal: string_field(extra, "principal").map(str::to_string),
                    credentials,
                },
                None => AuthToken::Unknown,
            },
            Some("bearer") => match credentials {
                Some(credentials) => AuthToken::Bearer { credentials },
                None => AuthToken::Unknown,
            },
            Some("none") => AuthToken::None,
            _ => AuthToken::Unknown,
        };
        Ok(Hello { user_agent, auth })
    }
}

/// Who a connection is, after `HELLO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The full-access token.
    Full,
    /// A role-bound token. The connection resolves the role's mask from the
    /// store at use, as the HTTP layer does.
    Role(String),
    /// `scheme: none`, with unauthenticated access turned on.
    Anonymous,
}

/// Checks whether a role still exists in the store.
pub type RoleExists = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Resolves Bolt credentials with the server's tokens: the same two sets the
/// HTTP `Authorization: Bearer` path checks.
#[derive(Clone)]
pub struct TokenAuthenticator {
    full_token: Option<String>,
    role_tokens: HashMap<String, String>,
    allow_unauthenticated: bool,
    role_exists: Option<RoleExists>,
}

impl std::fmt::Debug for TokenAuthenticator {
    // Never prints a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenAuthenticator")
            .field("role_tokens", &self.role_tokens.len())
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .finish_non_exhaustive()
    }
}

/// Byte equality that does not stop at the first difference, so response timing
/// does not reveal how much of a token matched.
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

impl TokenAuthenticator {
    /// `full_token` is the full-access token, `role_tokens` maps a token to a
    /// role name. `allow_unauthenticated` must come from an explicit flag; with
    /// it off, `scheme: none` is refused whatever tokens are configured.
    pub fn new(
        full_token: Option<String>,
        role_tokens: HashMap<String, String>,
        allow_unauthenticated: bool,
    ) -> Self {
        Self {
            full_token: full_token.filter(|t| !t.is_empty()),
            role_tokens,
            allow_unauthenticated,
            role_exists: None,
        }
    }

    /// Refuses a token whose role is no longer in the store.
    pub fn with_role_check(mut self, role_exists: RoleExists) -> Self {
        self.role_exists = Some(role_exists);
        self
    }

    pub fn authenticate(&self, auth: &AuthToken) -> Result<Identity, BoltError> {
        match auth {
            AuthToken::Basic { credentials, .. } | AuthToken::Bearer { credentials } => {
                self.resolve_token(credentials)
            }
            AuthToken::None if self.allow_unauthenticated => Ok(Identity::Anonymous),
            AuthToken::None | AuthToken::Unknown => Err(BoltError::Unauthorized),
        }
    }

    fn resolve_token(&self, presented: &str) -> Result<Identity, BoltError> {
        if presented.is_empty() {
            return Err(BoltError::Unauthorized);
        }
        // Every candidate is compared, with no early exit, so the time taken
        // does not depend on which one matched.
        let full = self
            .full_token
            .as_deref()
            .is_some_and(|t| constant_time_eq(presented.as_bytes(), t.as_bytes()));
        let mut role: Option<&str> = None;
        for (token, role_name) in &self.role_tokens {
            if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                role = Some(role_name.as_str());
            }
        }
        if full {
            return Ok(Identity::Full);
        }
        match role {
            Some(name) if self.role_exists.as_ref().is_none_or(|exists| exists(name)) => {
                Ok(Identity::Role(name.to_string()))
            }
            _ => Err(BoltError::Unauthorized),
        }
    }
}

/// The `SUCCESS` reply to an accepted `HELLO`.
pub fn hello_success(connection_id: &str) -> ServerMessage {
    ServerMessage::Success(vec![
        (
            "server".into(),
            PackValue::String(format!("MushroomDB/{}", env!("CARGO_PKG_VERSION"))),
        ),
        (
            "connection_id".into(),
            PackValue::String(connection_id.into()),
        ),
        ("hints".into(), PackValue::Map(vec![])),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> Vec<PackValue> {
        vec![PackValue::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), PackValue::String(v.to_string())))
                .collect(),
        )]
    }

    fn auth() -> TokenAuthenticator {
        let mut roles = HashMap::new();
        roles.insert("role-token".to_string(), "analyst".to_string());
        TokenAuthenticator::new(Some("full-token".into()), roles, false)
    }

    #[test]
    fn basic_treats_credentials_as_the_token_and_ignores_principal() {
        let hello = Hello::from_fields(map(&[
            ("scheme", "basic"),
            ("principal", "anything-at-all"),
            ("credentials", "full-token"),
        ]))
        .unwrap();
        assert_eq!(auth().authenticate(&hello.auth), Ok(Identity::Full));
        let other_principal = Hello::from_fields(map(&[
            ("scheme", "basic"),
            ("principal", "someone-else"),
            ("credentials", "role-token"),
        ]))
        .unwrap();
        assert_eq!(
            auth().authenticate(&other_principal.auth),
            Ok(Identity::Role("analyst".into()))
        );
    }

    #[test]
    fn bearer_scheme_resolves_the_same_way() {
        let hello = Hello::from_fields(map(&[("scheme", "bearer"), ("credentials", "role-token")]))
            .unwrap();
        assert_eq!(
            auth().authenticate(&hello.auth),
            Ok(Identity::Role("analyst".into()))
        );
    }

    #[test]
    fn every_failed_login_is_the_same_error() {
        let cases = [
            map(&[("scheme", "basic"), ("credentials", "wrong")]),
            map(&[("scheme", "basic"), ("credentials", "")]),
            map(&[("scheme", "basic")]),
            map(&[("scheme", "bearer")]),
            map(&[("scheme", "kerberos"), ("credentials", "full-token")]),
            map(&[("credentials", "full-token")]),
            map(&[("scheme", "none")]),
        ];
        for fields in cases {
            let hello = Hello::from_fields(fields).unwrap();
            assert_eq!(
                auth().authenticate(&hello.auth),
                Err(BoltError::Unauthorized),
                "{hello:?}"
            );
        }
    }

    #[test]
    fn scheme_none_needs_the_explicit_flag() {
        let hello = Hello::from_fields(map(&[("scheme", "none")])).unwrap();
        let open = TokenAuthenticator::new(None, HashMap::new(), true);
        assert_eq!(open.authenticate(&hello.auth), Ok(Identity::Anonymous));
        // No tokens configured and no flag: still refused.
        let closed = TokenAuthenticator::new(None, HashMap::new(), false);
        assert_eq!(
            closed.authenticate(&hello.auth),
            Err(BoltError::Unauthorized)
        );
    }

    #[test]
    fn empty_full_token_never_matches() {
        let a = TokenAuthenticator::new(Some(String::new()), HashMap::new(), false);
        let hello = Hello::from_fields(map(&[("scheme", "basic"), ("credentials", "")])).unwrap();
        assert_eq!(a.authenticate(&hello.auth), Err(BoltError::Unauthorized));
    }

    #[test]
    fn token_for_a_vanished_role_is_refused() {
        let a = auth().with_role_check(Arc::new(|name| name != "analyst"));
        let hello = Hello::from_fields(map(&[("scheme", "bearer"), ("credentials", "role-token")]))
            .unwrap();
        assert_eq!(a.authenticate(&hello.auth), Err(BoltError::Unauthorized));
    }

    #[test]
    fn unauthorized_message_names_no_token() {
        let err = BoltError::Unauthorized;
        assert!(!err.message().contains("token"));
        assert!(!format!("{:?}", auth()).contains("full-token"));
    }

    #[test]
    fn hello_needs_exactly_one_map() {
        assert!(Hello::from_fields(vec![]).is_err());
        assert!(Hello::from_fields(vec![PackValue::Null]).is_err());
        assert!(Hello::from_fields(vec![PackValue::Map(vec![]), PackValue::Null]).is_err());
    }

    #[test]
    fn user_agent_is_read() {
        let hello =
            Hello::from_fields(map(&[("user_agent", "driver/1"), ("scheme", "none")])).unwrap();
        assert_eq!(hello.user_agent.as_deref(), Some("driver/1"));
    }
}
