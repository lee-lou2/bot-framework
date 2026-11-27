//! Validation and normalization for upstream API endpoints (`providers.<alias>.endpoint`).
//!
//! Only `https://host[:port][/path]` is allowed; `http://` is restricted to loopback IP literals
//! (127.0.0.0/8, `[::1]`) because hostnames or private IPs could be rerouted over plaintext.
//! Credentials, queries, fragments, percent-encoding, whitespace, control characters, non-ASCII,
//! and alternate numeric/hex IP spellings are rejected.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const MAX_LEN: usize = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheme {
    Http,
    Https,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiEndpoint {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
    loopback: bool,
}

impl ApiEndpoint {
    /// Parses and validates an endpoint URL; errors use fixed messages without echoing the input.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty()
            || text.len() > MAX_LEN
            || !text.is_ascii()
            || text.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\')
        {
            return Err(
                "endpoint must be ASCII with no spaces, control characters, or backslashes"
                    .to_owned(),
            );
        }
        if text.contains(['?', '#', '@', '%']) {
            return Err(
                "credentials (user@), queries (?), fragments (#), and percent-encoding (%) are not allowed"
                    .to_owned(),
            );
        }
        let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = text.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(
                "endpoint must be `https://host[:port][/path]` (`http://` is allowed only for loopback IP literals)"
                    .to_owned(),
            );
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let (host, port) = split_authority(authority)?;
        let port = match port {
            Some(port) => port,
            None if scheme == Scheme::Https => 443,
            None => 80,
        };
        let loopback = matches!(&host, Host::Ip(ip) if ip.is_loopback());
        if scheme == Scheme::Http && !loopback {
            return Err(
                "`http://` is allowed only for loopback IP addresses (127.0.0.0/8, [::1]); other hosts must use `https://`"
                    .to_owned(),
            );
        }
        Ok(Self {
            scheme,
            host: match host {
                Host::Ip(ip) => ip.to_string(),
                Host::Name(name) => name,
            },
            port,
            path: base_path(path)?,
            loopback,
        })
    }

    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    pub fn url(&self, suffix: &str) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{scheme}://{host}:{}{}{suffix}", self.port, self.path)
    }
}

enum Host {
    Ip(IpAddr),
    Name(String),
}

fn split_authority(authority: &str) -> Result<(Host, Option<u16>), String> {
    let (host_text, port_text, bracketed) = if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, after)) = inner.split_once(']') else {
            return Err("unclosed bracket in IPv6 address".to_owned());
        };
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| "only `:port` may follow a closing IPv6 bracket".to_owned())?,
            ),
        };
        (host, port, true)
    } else {
        if authority.matches(':').count() > 1 {
            return Err("IPv6 addresses must be enclosed in brackets".to_owned());
        }
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port), false),
            None => (authority, None, false),
        }
    };
    let port = match port_text {
        None => None,
        Some(text) => {
            if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err("port must be an integer in 1..=65535".to_owned());
            }
            match text.parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => return Err("port must be an integer in 1..=65535".to_owned()),
            }
        }
    };
    let host = if bracketed {
        Host::Ip(IpAddr::V6(host_text.parse::<Ipv6Addr>().map_err(|_| {
            "bracketed host must be a valid IPv6 address".to_owned()
        })?))
    } else if let Ok(ip) = host_text.parse::<Ipv4Addr>() {
        Host::Ip(IpAddr::V4(ip))
    } else {
        Host::Name(dns_name(host_text).ok_or_else(|| {
            "host must be a valid DNS name (alphanumeric/hyphen, non-numeric final label) or IP address"
                .to_owned()
        })?)
    };
    Ok((host, port))
}

fn dns_name(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    if name.is_empty() || name.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    for label in &labels {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !ok {
            return None;
        }
    }
    // Reject all-digit or `0x` hex final labels that resolvers may interpret as alternate IP forms (e.g. `127.1`, `0x7f000001`).
    let last = *labels.last()?;
    if last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(name)
}

fn base_path(path: &str) -> Result<String, String> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    for segment in trimmed[1..].split('/') {
        let ok = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'));
        if !ok {
            return Err(
                "path segments must contain only ASCII alphanumeric or `-_.~` and cannot be empty, `.`, or `..`"
                    .to_owned(),
            );
        }
    }
    Ok(trimmed.to_owned())
}
