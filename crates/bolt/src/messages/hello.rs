//! `HELLO` and the mapping from Bolt credentials to a mushroomdb identity.
//!
//! At Bolt 5.0 and 4.4 the client authenticates inside `HELLO` (`LOGON` starts
//! at 5.1 and is not offered). The `HELLO` extra map carries `scheme`,
//! `principal` and `credentials`:
//!
//! - `basic`: `credentials` is a mushroomdb token. `principal` is ignored,
//!   because mushroomdb has no user table: a token carries its role.
//! - `bearer`: `credentials` is a token, the same way.
//! - `none`: no credentials; accepted only when the server has no tokens
//!   configured.
//!
//! Acceptance mirrors the HTTP layer exactly. With no full-access token and no
//! role-bound tokens configured, every client is `Full`, whatever scheme and
//! credentials it sends (HTTP accepts a request with any header). With any token
//! configured, a client must present one that matches.
//!
//! Every way of failing returns the same [`BoltError::Unauthorized`], so a
//! failed login does not say whether a token exists, whether its role is gone,
//! or which part of the request was wrong.

use std::collections::HashMap;
use std::sync::Arc;

use super::{PackValue, ServerMessage};
use crate::error::BoltError;

/// How the client says it authenticates. `Debug` hides the credentials.
#[derive(Clone, PartialEq, Eq)]
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

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthToken::Basic { principal, .. } => f
                .debug_struct("Basic")
                .field("principal", principal)
                .field("credentials", &"<redacted>")
                .finish(),
            AuthToken::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("credentials", &"<redacted>")
                .finish(),
            AuthToken::None => f.write_str("None"),
            AuthToken::Unknown => f.write_str("Unknown"),
        }
    }
}

/// A decoded `HELLO`. `Debug` hides the credentials.
#[derive(Clone, PartialEq)]
pub struct Hello {
    pub user_agent: Option<String>,
    pub auth: AuthToken,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("user_agent", &self.user_agent)
            .field("auth", &self.auth)
            .finish()
    }
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
}

/// Checks whether a role still exists in the store.
pub type RoleExists = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Resolves Bolt credentials with the server's tokens: the same two sets the
/// HTTP `Authorization: Bearer` path checks.
#[derive(Clone)]
pub struct TokenAuthenticator {
    full_token: Option<String>,
    role_tokens: HashMap<String, String>,
    role_exists: Option<RoleExists>,
}

impl std::fmt::Debug for TokenAuthenticator {
    // Never prints a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenAuthenticator")
            .field("role_tokens", &self.role_tokens.len())
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
    /// `full_token` is the full-access token (an empty one counts as none),
    /// `role_tokens` maps a token to a role name. With neither configured the
    /// server is open, as the HTTP layer is.
    pub fn new(full_token: Option<String>, role_tokens: HashMap<String, String>) -> Self {
        Self {
            full_token: full_token.filter(|t| !t.is_empty()),
            role_tokens,
            role_exists: None,
        }
    }

    /// Refuses a token whose role is no longer in the store.
    pub fn with_role_check(mut self, role_exists: RoleExists) -> Self {
        self.role_exists = Some(role_exists);
        self
    }

    /// Whether no token of either kind is configured.
    pub fn is_open(&self) -> bool {
        self.full_token.is_none() && self.role_tokens.is_empty()
    }

    pub fn authenticate(&self, auth: &AuthToken) -> Result<Identity, BoltError> {
        // Same rule as the HTTP middleware: no tokens configured, everyone is Full.
        if self.is_open() {
            return Ok(Identity::Full);
        }
        match auth {
            AuthToken::Basic { credentials, .. } | AuthToken::Bearer { credentials } => {
                self.resolve_token(credentials)
            }
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
        TokenAuthenticator::new(Some("full-token".into()), roles)
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

    /// The four combinations the HTTP layer defines: with no tokens configured
    /// every client is Full; with tokens, only a matching token is accepted.
    #[test]
    fn acceptance_matches_http_for_open_and_token_servers() {
        let none = Hello::from_fields(map(&[("scheme", "none")])).unwrap().auth;
        let valid = Hello::from_fields(map(&[("scheme", "basic"), ("credentials", "full-token")]))
            .unwrap()
            .auth;
        let invalid = Hello::from_fields(map(&[("scheme", "basic"), ("credentials", "bad")]))
            .unwrap()
            .auth;
        let open = TokenAuthenticator::new(None, HashMap::new());
        assert!(open.is_open());
        assert_eq!(open.authenticate(&none), Ok(Identity::Full));
        assert_eq!(open.authenticate(&valid), Ok(Identity::Full));
        assert_eq!(open.authenticate(&invalid), Ok(Identity::Full));
        let tokens = auth();
        assert!(!tokens.is_open());
        assert_eq!(tokens.authenticate(&none), Err(BoltError::Unauthorized));
        assert_eq!(tokens.authenticate(&valid), Ok(Identity::Full));
        assert_eq!(tokens.authenticate(&invalid), Err(BoltError::Unauthorized));
        // Role-only configuration is not open either.
        let mut roles = HashMap::new();
        roles.insert("t".to_string(), "r".to_string());
        let role_only = TokenAuthenticator::new(None, roles);
        assert_eq!(role_only.authenticate(&none), Err(BoltError::Unauthorized));
    }

    #[test]
    fn empty_full_token_counts_as_no_token() {
        let a = TokenAuthenticator::new(Some(String::new()), HashMap::new());
        assert!(a.is_open());
    }

    #[test]
    fn debug_output_hides_credentials() {
        let hello = Hello::from_fields(map(&[
            ("scheme", "basic"),
            ("principal", "who"),
            ("credentials", "super-secret-token"),
        ]))
        .unwrap();
        let shown = format!("{hello:?} {:?}", hello.auth);
        assert!(!shown.contains("super-secret-token"), "{shown}");
        assert!(shown.contains("<redacted>"));
        let bearer = Hello::from_fields(map(&[
            ("scheme", "bearer"),
            ("credentials", "super-secret-token"),
        ]))
        .unwrap();
        assert!(!format!("{bearer:?}").contains("super-secret-token"));
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
