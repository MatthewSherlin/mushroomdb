//! `serve`'s Bolt listener settings: the `--bolt-*` flags, their environment
//! variables, and the rules that turn them into a listener config or refuse the
//! start.
//!
//! The rules, all checked before the store is opened:
//!
//! - **Off by default.** No listener unless `--bolt-port` or
//!   `MUSHROOMDB_BOLT_PORT` gives a port. Another `--bolt-*` flag without a
//!   port is an error, not a silent no-op.
//! - **The bind address** is `--addr`'s IP with the Bolt port, so the
//!   non-loopback authentication rule `serve` applies to `--addr` covers the
//!   Bolt listener the same way.
//! - **TLS** follows HTTP: with `--tls-cert`/`--tls-key` given, Bolt is TLS
//!   unless `--bolt-plaintext` (or `MUSHROOMDB_BOLT_TLS=off`) says otherwise;
//!   without them it is plaintext, and `--bolt-tls` is refused.
//! - **ROUTE's address** is `--bolt-advertised-host` (else the bind IP) with
//!   `--bolt-advertised-port` (else the bind port). A wildcard bind
//!   (`0.0.0.0`, `::`) with no advertised host is refused: a driver told to
//!   dial `0.0.0.0` dials itself.

use std::net::SocketAddr;
use std::time::Duration;

use bolt::messages::route::{RoutingConfig, DEFAULT_STORE_NAME};

/// Environment variable for `--bolt-port`.
pub const BOLT_PORT_ENV: &str = "MUSHROOMDB_BOLT_PORT";
/// Environment variable for `--bolt-tls` / `--bolt-plaintext`: `on` or `off`.
pub const BOLT_TLS_ENV: &str = "MUSHROOMDB_BOLT_TLS";
/// Environment variable for `--bolt-advertised-host`.
pub const BOLT_ADVERTISED_HOST_ENV: &str = "MUSHROOMDB_BOLT_ADVERTISED_HOST";
/// Environment variable for `--bolt-advertised-port`.
pub const BOLT_ADVERTISED_PORT_ENV: &str = "MUSHROOMDB_BOLT_ADVERTISED_PORT";
/// Environment variable for `--bolt-max-connections`.
pub const BOLT_MAX_CONNECTIONS_ENV: &str = "MUSHROOMDB_BOLT_MAX_CONNECTIONS";

/// Default for `--bolt-tx-timeout-secs`.
pub const DEFAULT_BOLT_TX_TIMEOUT_SECS: u64 = 30;
/// Default for `--bookmark-timeout-secs`.
pub const DEFAULT_BOOKMARK_TIMEOUT_SECS: u64 = 5;

/// The `--bolt-*` flags as given; `None` is "not on the command line".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoltFlags {
    /// `--bolt-port <port>`.
    pub port: Option<u16>,
    /// `--bolt-tls` is `Some(true)`, `--bolt-plaintext` is `Some(false)`.
    pub tls: Option<bool>,
    /// `--bolt-advertised-host <host>`.
    pub advertised_host: Option<String>,
    /// `--bolt-advertised-port <port>`.
    pub advertised_port: Option<u16>,
    /// `--bolt-database <name>`.
    pub database: Option<String>,
    /// `--bolt-max-connections <n>`.
    pub max_connections: Option<usize>,
    /// `--bolt-tx-timeout-secs <secs>`.
    pub tx_timeout: Option<Duration>,
    /// `--bookmark-timeout-secs <secs>`.
    pub bookmark_timeout: Option<Duration>,
}

impl BoltFlags {
    /// Whether any flag other than the port was given.
    fn any_besides_port(&self) -> bool {
        self.tls.is_some()
            || self.advertised_host.is_some()
            || self.advertised_port.is_some()
            || self.database.is_some()
            || self.max_connections.is_some()
            || self.tx_timeout.is_some()
            || self.bookmark_timeout.is_some()
    }
}

/// A Bolt listener to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoltSettings {
    /// `--addr`'s IP with the Bolt port.
    pub bind: SocketAddr,
    pub tls: bool,
    pub advertised_host: Option<String>,
    pub advertised_port: Option<u16>,
    /// The store's name for `ROUTE` and database selection.
    pub database: String,
    pub max_connections: usize,
    pub tx_timeout: Duration,
    pub bookmark_timeout: Duration,
}

impl BoltSettings {
    /// The routing config for the address the listener actually bound (port 0
    /// picks a free one).
    pub fn routing(&self, bound: SocketAddr) -> Result<RoutingConfig, String> {
        let routing = RoutingConfig::new(bound, self.advertised_host.as_deref(), &self.database)
            .map_err(|e| advertise_error(bound, &e.to_string()))?;
        Ok(match self.advertised_port {
            Some(port) => routing.with_advertised_port(port),
            None => routing,
        })
    }

    /// `bolt` or `bolt+s`, the scheme a client uses to reach this listener.
    pub fn scheme(&self) -> &'static str {
        if self.tls {
            "bolt+s"
        } else {
            "bolt"
        }
    }
}

fn advertise_error(bind: SocketAddr, why: &str) -> String {
    format!(
        "--bolt-port on {bind}: {why}; give --bolt-advertised-host (or \
         {BOLT_ADVERTISED_HOST_ENV}), the host drivers reach this server by, \
         which ROUTE hands them"
    )
}

/// Reads a variable, treating an empty value as unset.
fn env_value(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_env<T: std::str::FromStr>(
    env: &dyn Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<T>, String> {
    match env_value(env, name) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .map(Some)
            .map_err(|_| format!("{name}={text:?} is not valid")),
    }
}

/// `on`/`off` and the usual spellings of each.
fn parse_switch(name: &str, text: &str) -> Result<bool, String> {
    match text.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err(format!(
            "{name}={text:?} is not recognised; use \"on\" or \"off\""
        )),
    }
}

/// Resolves the flags against the environment (`env`) and the HTTP settings:
/// `addr` is `--addr`, `tls_configured` whether `--tls-cert`/`--tls-key` were
/// given. `Ok(None)` is "no Bolt listener".
pub fn resolve_bolt_settings(
    flags: &BoltFlags,
    env: &dyn Fn(&str) -> Option<String>,
    addr: SocketAddr,
    tls_configured: bool,
) -> Result<Option<BoltSettings>, String> {
    let port = match flags.port {
        Some(port) => Some(port),
        None => parse_env::<u16>(env, BOLT_PORT_ENV)?,
    };
    let Some(port) = port else {
        if flags.any_besides_port() {
            return Err(format!(
                "the --bolt-* flags need --bolt-port (or {BOLT_PORT_ENV}); without a port no \
                 Bolt listener is started"
            ));
        }
        return Ok(None);
    };
    let tls = match flags.tls {
        Some(tls) => Some(tls),
        None => env_value(env, BOLT_TLS_ENV)
            .map(|text| parse_switch(BOLT_TLS_ENV, &text))
            .transpose()?,
    };
    let tls = match tls {
        Some(true) if !tls_configured => {
            return Err(
                "--bolt-tls needs --tls-cert and --tls-key: Bolt serves the HTTP certificate"
                    .into(),
            )
        }
        Some(explicit) => explicit,
        // TLS when HTTP has it: plaintext Bolt beside TLS HTTP takes asking.
        None => tls_configured,
    };
    let advertised_host = match &flags.advertised_host {
        Some(host) => Some(host.clone()),
        None => env_value(env, BOLT_ADVERTISED_HOST_ENV),
    };
    let advertised_port = match flags.advertised_port {
        Some(port) => Some(port),
        None => parse_env::<u16>(env, BOLT_ADVERTISED_PORT_ENV)?,
    };
    if advertised_port == Some(0) {
        return Err("--bolt-advertised-port must be a port from 1 to 65535".into());
    }
    let max_connections = match flags.max_connections {
        Some(n) => n,
        None => parse_env::<usize>(env, BOLT_MAX_CONNECTIONS_ENV)?
            .unwrap_or(bolt::server::DEFAULT_MAX_CONNECTIONS),
    };
    if max_connections == 0 {
        return Err("--bolt-max-connections must be at least 1".into());
    }
    let settings = BoltSettings {
        bind: SocketAddr::new(addr.ip(), port),
        tls,
        advertised_host,
        advertised_port,
        database: flags
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_STORE_NAME.to_string()),
        max_connections,
        tx_timeout: flags
            .tx_timeout
            .unwrap_or(Duration::from_secs(DEFAULT_BOLT_TX_TIMEOUT_SECS)),
        bookmark_timeout: flags
            .bookmark_timeout
            .unwrap_or(Duration::from_secs(DEFAULT_BOOKMARK_TIMEOUT_SECS)),
    };
    // Refuse an address ROUTE cannot advertise now, before the store opens.
    settings.routing(settings.bind)?;
    Ok(Some(settings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn loopback() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    fn with_port(port: u16) -> BoltFlags {
        BoltFlags {
            port: Some(port),
            ..BoltFlags::default()
        }
    }

    #[test]
    fn no_port_means_no_listener() {
        let none = resolve_bolt_settings(&BoltFlags::default(), &env(&[]), loopback(), false);
        assert_eq!(none, Ok(None));
        // An empty variable is unset.
        let empty = resolve_bolt_settings(
            &BoltFlags::default(),
            &env(&[(BOLT_PORT_ENV, "")]),
            loopback(),
            false,
        );
        assert_eq!(empty, Ok(None));
    }

    #[test]
    fn another_bolt_flag_without_a_port_is_refused() {
        let flags = BoltFlags {
            tls: Some(false),
            ..BoltFlags::default()
        };
        let err = resolve_bolt_settings(&flags, &env(&[]), loopback(), false).unwrap_err();
        assert!(err.contains("--bolt-port"), "{err}");
    }

    #[test]
    fn the_port_comes_from_the_flag_or_the_environment_and_binds_the_http_ip() {
        let from_flag = resolve_bolt_settings(&with_port(7687), &env(&[]), loopback(), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            from_flag.bind,
            "127.0.0.1:7687".parse::<SocketAddr>().unwrap()
        );
        assert!(!from_flag.tls);
        assert_eq!(from_flag.database, "mushroomdb");
        assert_eq!(
            from_flag.max_connections,
            bolt::server::DEFAULT_MAX_CONNECTIONS
        );
        assert_eq!(from_flag.tx_timeout, Duration::from_secs(30));
        assert_eq!(from_flag.bookmark_timeout, Duration::from_secs(5));
        let from_env = resolve_bolt_settings(
            &BoltFlags::default(),
            &env(&[(BOLT_PORT_ENV, "7699")]),
            loopback(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(from_env.bind.port(), 7699);
        // The flag wins over the variable.
        let both = resolve_bolt_settings(
            &with_port(7687),
            &env(&[(BOLT_PORT_ENV, "7699")]),
            loopback(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(both.bind.port(), 7687);
        let bad = resolve_bolt_settings(
            &BoltFlags::default(),
            &env(&[(BOLT_PORT_ENV, "seventy")]),
            loopback(),
            false,
        );
        assert!(bad.unwrap_err().contains(BOLT_PORT_ENV));
    }

    #[test]
    fn tls_follows_http_unless_plaintext_is_asked_for() {
        let resolve = |flags: &BoltFlags, vars: &[(&str, &str)], tls_configured: bool| {
            resolve_bolt_settings(flags, &env(vars), loopback(), tls_configured)
        };
        // HTTP has TLS: Bolt does too, by default.
        assert!(resolve(&with_port(7687), &[], true).unwrap().unwrap().tls);
        // Explicit plaintext beside TLS HTTP, by flag or variable.
        let plaintext = BoltFlags {
            tls: Some(false),
            ..with_port(7687)
        };
        assert!(!resolve(&plaintext, &[], true).unwrap().unwrap().tls);
        assert!(
            !resolve(&with_port(7687), &[(BOLT_TLS_ENV, "off")], true)
                .unwrap()
                .unwrap()
                .tls
        );
        // No HTTP TLS: plaintext, and asking for TLS is refused.
        assert!(!resolve(&with_port(7687), &[], false).unwrap().unwrap().tls);
        let tls = BoltFlags {
            tls: Some(true),
            ..with_port(7687)
        };
        assert!(resolve(&tls, &[], false)
            .unwrap_err()
            .contains("--tls-cert"));
        assert!(resolve(&with_port(7687), &[(BOLT_TLS_ENV, "on")], false).is_err());
        assert!(resolve(&with_port(7687), &[(BOLT_TLS_ENV, "maybe")], true)
            .unwrap_err()
            .contains(BOLT_TLS_ENV));
        // The flag wins over the variable.
        assert!(
            resolve(&tls, &[(BOLT_TLS_ENV, "off")], true)
                .unwrap()
                .unwrap()
                .tls
        );
    }

    #[test]
    fn a_wildcard_bind_needs_an_advertised_host() {
        let wildcard: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        let err = resolve_bolt_settings(&with_port(7687), &env(&[]), wildcard, false).unwrap_err();
        assert!(err.contains("--bolt-advertised-host"), "{err}");
        let v6: SocketAddr = "[::]:8080".parse().unwrap();
        assert!(resolve_bolt_settings(&with_port(7687), &env(&[]), v6, false).is_err());
        // Given by flag or variable, it is advertised with the bind port...
        let flags = BoltFlags {
            advertised_host: Some("db.example.test".into()),
            ..with_port(7687)
        };
        let settings = resolve_bolt_settings(&flags, &env(&[]), wildcard, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            settings
                .routing(settings.bind)
                .unwrap()
                .advertised_address(),
            "db.example.test:7687"
        );
        let from_env = resolve_bolt_settings(
            &with_port(7687),
            &env(&[
                (BOLT_ADVERTISED_HOST_ENV, "db.example.test"),
                (BOLT_ADVERTISED_PORT_ENV, "17687"),
            ]),
            wildcard,
            false,
        )
        .unwrap()
        .unwrap();
        // ...or with the advertised port, for a port-mapped container.
        assert_eq!(
            from_env
                .routing(from_env.bind)
                .unwrap()
                .advertised_address(),
            "db.example.test:17687"
        );
        // A wildcard is not an advertised host either.
        let wild_host = BoltFlags {
            advertised_host: Some("0.0.0.0".into()),
            ..with_port(7687)
        };
        assert!(resolve_bolt_settings(&wild_host, &env(&[]), wildcard, false).is_err());
    }

    #[test]
    fn a_loopback_bind_advertises_itself_and_port_zero_takes_the_bound_port() {
        let settings = resolve_bolt_settings(&with_port(0), &env(&[]), loopback(), false)
            .unwrap()
            .unwrap();
        let bound: SocketAddr = "127.0.0.1:54321".parse().unwrap();
        assert_eq!(
            settings.routing(bound).unwrap().advertised_address(),
            "127.0.0.1:54321"
        );
    }

    #[test]
    fn limits_and_names_are_checked() {
        let zero = BoltFlags {
            max_connections: Some(0),
            ..with_port(7687)
        };
        assert!(resolve_bolt_settings(&zero, &env(&[]), loopback(), false).is_err());
        let from_env = resolve_bolt_settings(
            &with_port(7687),
            &env(&[(BOLT_MAX_CONNECTIONS_ENV, "3")]),
            loopback(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(from_env.max_connections, 3);
        let blank_name = BoltFlags {
            database: Some("  ".into()),
            ..with_port(7687)
        };
        assert!(resolve_bolt_settings(&blank_name, &env(&[]), loopback(), false).is_err());
        let zero_port = BoltFlags {
            advertised_port: Some(0),
            ..with_port(7687)
        };
        assert!(resolve_bolt_settings(&zero_port, &env(&[]), loopback(), false).is_err());
    }

    #[test]
    fn the_scheme_names_tls() {
        let mut settings = resolve_bolt_settings(&with_port(7687), &env(&[]), loopback(), false)
            .unwrap()
            .unwrap();
        assert_eq!(settings.scheme(), "bolt");
        settings.tls = true;
        assert_eq!(settings.scheme(), "bolt+s");
    }
}
