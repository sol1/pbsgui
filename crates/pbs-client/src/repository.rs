//! Parsing of a PBS repository string.
//!
//! A repository is written as `[[auth-id@]host[:port]:]datastore`, for example:
//!   - `backups`
//!   - `pbs.example.com:backups`
//!   - `pbs.example.com:8007:backups`
//!   - `svc@pbs!token@pbs.example.com:backups`
//!   - `root@pam@[2001:db8::1]:8007:backups`
//!
//! The auth id may itself contain `@` (it is `user@realm`, optionally with a
//! `!tokenname` API token suffix), so the host is separated at the last `@`.
//! The host part of the string never contains `@`.

use std::fmt;
use std::str::FromStr;

use crate::error::PbsError;

/// Default PBS API / backup protocol port.
pub const DEFAULT_PORT: u16 = 8007;

/// A parsed PBS repository reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// Auth id (`user@realm`, optionally `user@realm!token`). `None` when not given.
    pub auth_id: Option<String>,
    /// Server host or IP. `None` means a local datastore (rare for this tool).
    pub host: Option<String>,
    /// TCP port. `None` means the [`DEFAULT_PORT`].
    pub port: Option<u16>,
    /// Datastore name (required).
    pub datastore: String,
}

impl Repository {
    /// The effective port, falling back to [`DEFAULT_PORT`].
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }
}

impl FromStr for Repository {
    type Err = PbsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(PbsError::InvalidRepository("empty string".into()));
        }

        // Split the auth id (which may contain '@') from the host part at the
        // last '@'. The host part never contains '@'.
        let (auth_id, rest) = match s.rsplit_once('@') {
            Some((auth, rest)) => (Some(auth.to_string()), rest),
            None => (None, s),
        };

        let (host, port, datastore) = parse_host_part(rest)?;

        if datastore.is_empty() {
            return Err(PbsError::InvalidRepository(format!(
                "missing datastore in {s:?}"
            )));
        }

        Ok(Repository {
            auth_id,
            host,
            port,
            datastore,
        })
    }
}

/// Parse `host[:port]:datastore`, `host:datastore`, or `datastore`, with
/// optional bracketed IPv6 host (`[2001:db8::1]:port:datastore`).
fn parse_host_part(rest: &str) -> Result<(Option<String>, Option<u16>, String), PbsError> {
    if let Some(after_bracket) = rest.strip_prefix('[') {
        // Bracketed IPv6 host.
        let close = after_bracket
            .find(']')
            .ok_or_else(|| PbsError::InvalidRepository(format!("unclosed '[' in {rest:?}")))?;
        let host = &after_bracket[..close];
        let tail = after_bracket[close + 1..]
            .strip_prefix(':')
            .ok_or_else(|| {
                PbsError::InvalidRepository(format!("expected ':' after ']' in {rest:?}"))
            })?;
        let (port, datastore) = split_port_datastore(tail, rest)?;
        return Ok((Some(format!("[{host}]")), port, datastore));
    }

    let parts: Vec<&str> = rest.split(':').collect();
    match parts.as_slice() {
        [datastore] => Ok((None, None, (*datastore).to_string())),
        [host, datastore] => Ok((Some((*host).to_string()), None, (*datastore).to_string())),
        [host, port, datastore] => {
            let port = parse_port(port, rest)?;
            Ok((
                Some((*host).to_string()),
                Some(port),
                (*datastore).to_string(),
            ))
        }
        _ => Err(PbsError::InvalidRepository(format!(
            "too many ':' separated fields in {rest:?}"
        ))),
    }
}

/// Parse the tail after an IPv6 host: either `port:datastore` or `datastore`.
fn split_port_datastore(tail: &str, ctx: &str) -> Result<(Option<u16>, String), PbsError> {
    match tail.split_once(':') {
        Some((port, datastore)) => Ok((Some(parse_port(port, ctx)?), datastore.to_string())),
        None => Ok((None, tail.to_string())),
    }
}

fn parse_port(s: &str, ctx: &str) -> Result<u16, PbsError> {
    s.parse::<u16>()
        .map_err(|_| PbsError::InvalidRepository(format!("invalid port {s:?} in {ctx:?}")))
}

impl fmt::Display for Repository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(auth) = &self.auth_id {
            write!(f, "{auth}@")?;
        }
        if let Some(host) = &self.host {
            write!(f, "{host}")?;
            if let Some(port) = self.port {
                write!(f, ":{port}")?;
            }
            write!(f, ":")?;
        }
        write!(f, "{}", self.datastore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Repository {
        s.parse().unwrap()
    }

    #[test]
    fn datastore_only() {
        let r = parse("backups");
        assert_eq!(r.auth_id, None);
        assert_eq!(r.host, None);
        assert_eq!(r.datastore, "backups");
        assert_eq!(r.port(), DEFAULT_PORT);
    }

    #[test]
    fn host_and_datastore() {
        let r = parse("pbs.example.com:backups");
        assert_eq!(r.host.as_deref(), Some("pbs.example.com"));
        assert_eq!(r.port, None);
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn host_port_datastore() {
        let r = parse("pbs.example.com:8007:backups");
        assert_eq!(r.host.as_deref(), Some("pbs.example.com"));
        assert_eq!(r.port, Some(8007));
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn auth_with_realm() {
        let r = parse("root@pam@pbs.example.com:backups");
        assert_eq!(r.auth_id.as_deref(), Some("root@pam"));
        assert_eq!(r.host.as_deref(), Some("pbs.example.com"));
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn auth_token_port_datastore() {
        let r = parse("svc@pbs!tok@pbs.example.com:8007:backups");
        assert_eq!(r.auth_id.as_deref(), Some("svc@pbs!tok"));
        assert_eq!(r.host.as_deref(), Some("pbs.example.com"));
        assert_eq!(r.port, Some(8007));
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn ipv6_host() {
        let r = parse("[2001:db8::1]:backups");
        assert_eq!(r.host.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(r.port, None);
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn ipv6_host_with_port_and_auth() {
        let r = parse("root@pam@[2001:db8::1]:8007:backups");
        assert_eq!(r.auth_id.as_deref(), Some("root@pam"));
        assert_eq!(r.host.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(r.port, Some(8007));
        assert_eq!(r.datastore, "backups");
    }

    #[test]
    fn round_trips_via_display() {
        for s in [
            "backups",
            "pbs.example.com:backups",
            "pbs.example.com:8007:backups",
            "root@pam@pbs.example.com:8007:backups",
            "[2001:db8::1]:8007:backups",
        ] {
            assert_eq!(parse(s).to_string(), s, "round trip failed for {s}");
        }
    }

    #[test]
    fn rejects_empty_and_bad_port() {
        assert!("".parse::<Repository>().is_err());
        assert!("host:notaport:ds".parse::<Repository>().is_err());
        assert!("a:b:c:d".parse::<Repository>().is_err());
    }
}
