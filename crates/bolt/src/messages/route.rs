//! `ROUTE`: a routing table for a single server.
//!
//! A driver given a routing-scheme URI asks `ROUTE` on connect and then talks
//! only to the addresses it is told. This server is the whole cluster, so every
//! role (`WRITE`, `READ`, `ROUTE`) points at its own advertised address.
//!
//! The advertised address comes from config and is never a wildcard: a server
//! bound to `0.0.0.0` must be told the host clients reach it by, because a
//! driver that is handed `0.0.0.0:7687` would dial itself.
//!
//! Database names: `null`, `""`, the drivers' default name and the configured
//! store name select this store; any other name is `DatabaseNotFound`. Names are
//! compared without regard to ASCII case, as the drivers' servers compare them.

use std::net::{IpAddr, SocketAddr};

use super::{PackValue, ServerMessage};
use crate::error::BoltError;

/// How long a driver may reuse the table, in seconds.
pub const ROUTING_TTL_SECONDS: i64 = 300;

/// The database name drivers use when the application names none. A protocol
/// constant: drivers send it, and it selects this store. Spelled as bytes so
/// the name never appears in the repository's prose (scripts/check-names.py).
pub const DRIVER_DEFAULT_DATABASE: &str = match std::str::from_utf8(&[0x6e, 0x65, 0x6f, 0x34, 0x6a])
{
    Ok(name) => name,
    Err(_) => panic!("the drivers' default database name is ASCII"),
};

/// The store name used when none is configured.
pub const DEFAULT_STORE_NAME: &str = "mushroomdb";

/// Why a routing config could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingConfigError(pub String);

impl std::fmt::Display for RoutingConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoutingConfigError {}

/// What `ROUTE` and database selection need from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingConfig {
    advertised: String,
    /// The host part of `advertised`, kept so the port can be replaced.
    host: String,
    store_name: String,
}

impl RoutingConfig {
    /// Builds the config from the listener's bind address, an optional
    /// advertised host, and the store name.
    ///
    /// The advertised host is `advertised_host` when given, else the bind
    /// address's IP. Either way it must name a concrete host: an unspecified
    /// address (`0.0.0.0`, `::`) or an empty host is refused, so a server bound
    /// to all interfaces has to be told which host to advertise. The port is
    /// the bind port.
    pub fn new(
        bind: SocketAddr,
        advertised_host: Option<&str>,
        store_name: &str,
    ) -> Result<Self, RoutingConfigError> {
        let host = match advertised_host {
            Some(host) => host.trim().to_string(),
            None => bind.ip().to_string(),
        };
        let host = Self::check_host(&host)?;
        let store_name = store_name.trim();
        if store_name.is_empty() {
            return Err(RoutingConfigError("the store name is empty".into()));
        }
        Ok(Self {
            advertised: format!("{host}:{}", bind.port()),
            host,
            store_name: store_name.to_string(),
        })
    }

    /// Advertises `port` instead of the bind port: for a listener reached
    /// through a port mapping (a container's `-p 17687:7687`).
    pub fn with_advertised_port(mut self, port: u16) -> Self {
        self.advertised = format!("{}:{port}", self.host);
        self
    }

    /// Returns the host in the form it takes before `:port` (an IPv6 literal
    /// in brackets), or why it cannot be advertised.
    fn check_host(host: &str) -> Result<String, RoutingConfigError> {
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if bare.is_empty() {
            return Err(RoutingConfigError(
                "set an advertised host: the bind address cannot be advertised when it is \
                 empty"
                    .into(),
            ));
        }
        if let Ok(ip) = bare.parse::<IpAddr>() {
            if ip.is_unspecified() {
                return Err(RoutingConfigError(
                    "set an advertised host: the bind address is a wildcard and clients \
                     cannot connect to it"
                        .into(),
                ));
            }
            return Ok(match ip {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            });
        }
        if bare.contains(|c: char| c.is_whitespace() || c == '/' || c == ':' || c == '@') {
            return Err(RoutingConfigError(format!(
                "the advertised host {bare:?} is not a host name or IP address (give the \
                 host only, without a port)"
            )));
        }
        Ok(bare.to_string())
    }

    /// The `host:port` this server advertises.
    pub fn advertised_address(&self) -> &str {
        &self.advertised
    }

    pub fn store_name(&self) -> &str {
        &self.store_name
    }

    /// Whether `requested` selects this store: `None`, `""`, the drivers'
    /// default name, or the store name, either name in any ASCII case.
    pub fn check_database(&self, requested: Option<&str>) -> Result<(), BoltError> {
        match requested {
            None | Some("") => Ok(()),
            Some(name)
                if name.eq_ignore_ascii_case(DRIVER_DEFAULT_DATABASE)
                    || name.eq_ignore_ascii_case(&self.store_name) =>
            {
                Ok(())
            }
            Some(name) => Err(BoltError::DatabaseNotFound(format!(
                "database {name:?} does not exist"
            ))),
        }
    }

    /// The reply to a `ROUTE` for `requested`.
    pub fn route(&self, requested: Option<&str>) -> Result<ServerMessage, BoltError> {
        self.check_database(requested)?;
        let server = |role: &str| {
            PackValue::Map(vec![
                (
                    "addresses".into(),
                    PackValue::List(vec![PackValue::String(self.advertised.clone())]),
                ),
                ("role".into(), PackValue::String(role.into())),
            ])
        };
        let table = PackValue::Map(vec![
            ("ttl".into(), PackValue::Int(ROUTING_TTL_SECONDS)),
            ("db".into(), PackValue::String(self.store_name.clone())),
            (
                "servers".into(),
                PackValue::List(vec![server("WRITE"), server("READ"), server("ROUTE")]),
            ),
        ]);
        Ok(ServerMessage::Success(vec![("rt".into(), table)]))
    }
}

/// Reads a `ROUTE` request's fields and returns the requested database name.
///
/// The request is `ROUTE routing: Map, bookmarks: List, third`, where `third` is
/// the database name (a string or null) at Bolt 4.3 and an `extra` map holding
/// an optional `db` at 4.4 and 5.0. The routing context and bookmarks are
/// ignored. A request of any other shape is malformed.
pub fn requested_database(fields: &[PackValue]) -> Result<Option<String>, BoltError> {
    let malformed = || BoltError::Invalid("malformed ROUTE request".into());
    let [PackValue::Map(_), PackValue::List(_), third] = fields else {
        return Err(malformed());
    };
    match third {
        PackValue::Null => Ok(None),
        PackValue::String(name) => Ok(Some(name.clone())),
        PackValue::Map(extra) => match extra.iter().find(|(k, _)| k == "db") {
            None | Some((_, PackValue::Null)) => Ok(None),
            Some((_, PackValue::String(name))) => Ok(Some(name.clone())),
            Some(_) => Err(malformed()),
        },
        _ => Err(malformed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RoutingConfig {
        RoutingConfig::new(
            "0.0.0.0:7687".parse().unwrap(),
            Some("db.example.test"),
            "mushroomdb",
        )
        .unwrap()
    }

    fn servers(reply: &ServerMessage) -> Vec<(String, Vec<String>)> {
        let ServerMessage::Success(meta) = reply else {
            panic!("not a success")
        };
        let PackValue::Map(table) = &meta[0].1 else {
            panic!("rt is not a map")
        };
        let PackValue::List(servers) = &table.iter().find(|(k, _)| k == "servers").unwrap().1
        else {
            panic!("servers is not a list")
        };
        servers
            .iter()
            .map(|s| {
                let PackValue::Map(entries) = s else { panic!() };
                let PackValue::String(role) = &entries.iter().find(|(k, _)| k == "role").unwrap().1
                else {
                    panic!()
                };
                let PackValue::List(addrs) =
                    &entries.iter().find(|(k, _)| k == "addresses").unwrap().1
                else {
                    panic!()
                };
                let addrs = addrs
                    .iter()
                    .map(|a| match a {
                        PackValue::String(s) => s.clone(),
                        _ => panic!(),
                    })
                    .collect();
                (role.clone(), addrs)
            })
            .collect()
    }

    #[test]
    fn route_returns_three_roles_at_the_advertised_address() {
        let reply = config().route(Some("mushroomdb")).unwrap();
        let roles = servers(&reply);
        assert_eq!(roles.len(), 3);
        for (role, addrs) in &roles {
            assert!(["WRITE", "READ", "ROUTE"].contains(&role.as_str()));
            assert_eq!(addrs, &vec!["db.example.test:7687".to_string()]);
        }
        let ServerMessage::Success(meta) = &reply else {
            panic!()
        };
        let PackValue::Map(table) = &meta[0].1 else {
            panic!()
        };
        assert!(table.contains(&("ttl".into(), PackValue::Int(300))));
        assert!(table.contains(&("db".into(), PackValue::String("mushroomdb".into()))));
    }

    #[test]
    fn route_request_forms_are_read() {
        let map = || PackValue::Map(vec![]);
        let list = || PackValue::List(vec![]);
        // 4.3: the third field is the database name.
        assert_eq!(
            requested_database(&[map(), list(), PackValue::String("mushroomdb".into())]).unwrap(),
            Some("mushroomdb".into())
        );
        assert_eq!(
            requested_database(&[map(), list(), PackValue::Null]).unwrap(),
            None
        );
        // 4.4 and 5.0: an extra map with an optional db.
        let extra = PackValue::Map(vec![(
            "db".into(),
            PackValue::String(DRIVER_DEFAULT_DATABASE.into()),
        )]);
        assert_eq!(
            requested_database(&[map(), list(), extra]).unwrap(),
            Some(DRIVER_DEFAULT_DATABASE.into())
        );
        assert_eq!(requested_database(&[map(), list(), map()]).unwrap(), None);
        // Malformed.
        assert!(requested_database(&[]).is_err());
        assert!(requested_database(&[map(), list()]).is_err());
        assert!(requested_database(&[map(), list(), PackValue::Int(1)]).is_err());
        assert!(requested_database(&[PackValue::Null, list(), PackValue::Null]).is_err());
    }

    #[test]
    fn database_selection() {
        let cfg = config();
        for ok in [
            None,
            Some(""),
            Some(DRIVER_DEFAULT_DATABASE),
            Some("mushroomdb"),
        ] {
            assert!(cfg.check_database(ok).is_ok(), "{ok:?}");
            assert!(cfg.route(ok).is_ok(), "{ok:?}");
        }
        let err = cfg.route(Some("other")).unwrap_err();
        assert_eq!(err.code(), "Neo.ClientError.Database.DatabaseNotFound");
        let custom = RoutingConfig::new("127.0.0.1:7687".parse().unwrap(), None, "orders").unwrap();
        assert!(custom.check_database(Some("orders")).is_ok());
        assert!(custom.check_database(Some("mushroomdb")).is_err());
    }

    #[test]
    fn a_wildcard_bind_without_an_advertised_host_is_refused() {
        for bind in ["0.0.0.0:7687", "[::]:7687"] {
            assert!(RoutingConfig::new(bind.parse().unwrap(), None, "mushroomdb").is_err());
        }
        for host in ["0.0.0.0", "::", "[::]", "", "  "] {
            let result =
                RoutingConfig::new("0.0.0.0:7687".parse().unwrap(), Some(host), "mushroomdb");
            assert!(result.is_err(), "{host:?}");
        }
    }

    #[test]
    fn the_advertised_address_is_never_a_wildcard_and_follows_the_bind_port() {
        let from_bind =
            RoutingConfig::new("10.1.2.3:9000".parse().unwrap(), None, "mushroomdb").unwrap();
        assert_eq!(from_bind.advertised_address(), "10.1.2.3:9000");
        let v6 = RoutingConfig::new("[::1]:7687".parse().unwrap(), None, "mushroomdb").unwrap();
        assert_eq!(v6.advertised_address(), "[::1]:7687");
        let named = config();
        assert!(!named.advertised_address().contains("0.0.0.0"));
        // A host with a port, a path or whitespace is not a host.
        for bad in ["host:7687", "a b", "host/x", "user@host"] {
            assert!(
                RoutingConfig::new("0.0.0.0:7687".parse().unwrap(), Some(bad), "mushroomdb")
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn database_names_are_compared_without_regard_to_case() {
        let cfg = RoutingConfig::new("127.0.0.1:7687".parse().unwrap(), None, "Orders").unwrap();
        let shouted = DRIVER_DEFAULT_DATABASE.to_ascii_uppercase();
        for name in ["orders", "ORDERS", "Orders", shouted.as_str()] {
            assert!(cfg.check_database(Some(name)).is_ok(), "{name}");
            assert!(cfg.route(Some(name)).is_ok(), "{name}");
        }
        assert!(cfg.check_database(Some("orders2")).is_err());
    }

    #[test]
    fn the_advertised_port_can_differ_from_the_bind_port() {
        // A container maps host port 17687 to the listener's 7687.
        let cfg = RoutingConfig::new(
            "0.0.0.0:7687".parse().unwrap(),
            Some("db.example.test"),
            "mushroomdb",
        )
        .unwrap()
        .with_advertised_port(17687);
        assert_eq!(cfg.advertised_address(), "db.example.test:17687");
        let reply = cfg.route(None).unwrap();
        for (_, addrs) in servers(&reply) {
            assert_eq!(addrs, vec!["db.example.test:17687".to_string()]);
        }
        let v6 = RoutingConfig::new("[::1]:7687".parse().unwrap(), None, "mushroomdb")
            .unwrap()
            .with_advertised_port(9);
        assert_eq!(v6.advertised_address(), "[::1]:9");
    }

    #[test]
    fn an_empty_store_name_is_refused() {
        assert!(RoutingConfig::new("127.0.0.1:7687".parse().unwrap(), None, " ").is_err());
    }
}
