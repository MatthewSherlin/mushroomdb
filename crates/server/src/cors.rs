//! CORS for the HTTP server, configured by `MUSHROOMDB_CORS_ORIGINS`.
//!
//! Unset or blank means no CORS layer at all: responses are byte-identical to
//! a server built before this existed. `*` allows any origin and never allows
//! credentials (browsers reject the combination). A comma-separated list of
//! explicit origins allows exactly those, with credentials.
//!
//! The layer is outermost, so a preflight `OPTIONS` is answered before the auth
//! middleware runs (browsers send no `Authorization` on a preflight). A
//! preflight grants nothing: the actual request that follows is still
//! authenticated, and its 401 carries the CORS headers so a page can read it.

use axum::http::{header, HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// The environment variable holding the allowed origins.
pub const CORS_ORIGINS_ENV: &str = "MUSHROOMDB_CORS_ORIGINS";

/// Parse a `MUSHROOMDB_CORS_ORIGINS` value. `Ok(None)` for unset/blank.
///
/// Errors name the offending entry so a misconfigured deploy fails at startup
/// with something actionable, rather than serving with CORS silently off.
pub fn parse_cors_origins(spec: &str) -> Result<Option<CorsLayer>, String> {
    let entries: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if entries.is_empty() {
        return Ok(None);
    }
    if entries.contains(&"*") {
        if entries.len() > 1 {
            return Err(format!(
                "{CORS_ORIGINS_ENV}: `*` cannot be combined with other origins"
            ));
        }
        return Ok(Some(CorsLayer::permissive()));
    }
    let origins = entries
        .iter()
        .map(|entry| parse_origin(entry))
        .collect::<Result<Vec<HeaderValue>, String>>()?;
    Ok(Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                HeaderName::from_static("x-request-id"),
            ])
            .allow_credentials(true),
    ))
}

/// Read and parse [`CORS_ORIGINS_ENV`] from the process environment.
pub fn cors_layer_from_env() -> Result<Option<CorsLayer>, String> {
    match std::env::var(CORS_ORIGINS_ENV) {
        Ok(spec) => parse_cors_origins(&spec),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(format!("{CORS_ORIGINS_ENV}: value is not valid UTF-8"))
        }
    }
}

/// An origin is `scheme://host[:port]`: no path, query, or trailing slash.
fn parse_origin(entry: &str) -> Result<HeaderValue, String> {
    let bad = |why: &str| format!("{CORS_ORIGINS_ENV}: invalid origin `{entry}`: {why}");
    let rest = entry
        .strip_prefix("http://")
        .or_else(|| entry.strip_prefix("https://"))
        .ok_or_else(|| bad("must start with http:// or https://"))?;
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@', '*'])
        || rest.contains(char::is_whitespace)
    {
        return Err(bad(
            "must be scheme://host[:port] with no path, trailing slash or wildcard",
        ));
    }
    HeaderValue::from_str(entry).map_err(|_| bad("not a valid header value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_is_none() {
        assert!(parse_cors_origins("").unwrap().is_none());
        assert!(parse_cors_origins("  , ").unwrap().is_none());
    }

    #[test]
    fn rejects_bad_origins() {
        for bad in [
            "example.com",
            "https://example.com/",
            "https://example.com/path",
            "ftp://example.com",
            "https://",
            "*,https://a.com",
            "https://*.example.com",
        ] {
            assert!(parse_cors_origins(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn accepts_lists_and_wildcard() {
        assert!(parse_cors_origins("*").unwrap().is_some());
        assert!(parse_cors_origins("https://a.com, http://localhost:3000")
            .unwrap()
            .is_some());
    }
}
