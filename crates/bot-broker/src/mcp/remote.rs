//! Remote Streamable HTTP MCP server endpoint parsing, validation, and revision hashing.
//!
//! - Requires `https` except for loopback IPv4 literals (`127.0.0.0/8`); IPv6 literals, userinfo, and fragments are rejected.
//! - Rejects WHATWG "ends in a number" non-canonical numeric hosts (hex/octal/integer/short IPv4/trailing dot) so URL parsers and OS resolvers cannot bypass hostname egress checks.
//! - Connections target only this `host:port` with redirects disabled.

use crate::{egress::canon_host, resolve::HostPort};
use bot_core::{
    auth::{AuthSource, CredentialKind},
    id::SecretName,
};
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;

const MAX_URL: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remote {
    /// True for `https`; `false` is constructed only for loopback IPv4 literals.
    pub tls: bool,
    /// Normalized lowercase hostname or canonical dotted-decimal IPv4 address.
    pub host: String,
    pub port: u16,
    /// Path and query beginning with `/`.
    pub path: String,
    /// Profile-scoped static bearer secret name sent only in the Authorization header when set.
    pub bearer: Option<SecretName>,
    /// Managed host OAuth credential name (mutually exclusive with `bearer`).
    pub oauth: Option<SecretName>,
    /// Optional pinned OAuth issuer URL (`https://...`) bound into approvals for OAuth remotes.
    pub oauth_issuer: Option<String>,
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

impl Remote {
    pub fn parse(url: &str, bearer: Option<SecretName>) -> Result<Self, String> {
        Self::parse_parts(url, bearer, None, None)
    }

    pub fn parse_oauth(url: &str, credential: SecretName) -> Result<Self, String> {
        Self::parse_parts(url, None, Some(credential), None)
    }

    pub fn parse_oauth_pinned(
        url: &str,
        credential: SecretName,
        issuer: Option<&str>,
    ) -> Result<Self, String> {
        let pinned = match issuer {
            Some(raw) => {
                let parsed = Self::parse(raw, None)
                    .map_err(|why| format!("oauth issuer is invalid: {why}"))?;
                if !parsed.tls {
                    return Err("oauth issuer must use https".to_owned());
                }
                Some(parsed.url().trim_end_matches('/').to_owned())
            }
            None => None,
        };
        Self::parse_parts(url, None, Some(credential), pinned)
    }

    fn parse_parts(
        url: &str,
        bearer: Option<SecretName>,
        oauth: Option<SecretName>,
        oauth_issuer: Option<String>,
    ) -> Result<Self, String> {
        if url.is_empty()
            || url.len() > MAX_URL
            || !url.is_ascii()
            || url.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(format!(
                "url must be ASCII without spaces or control chars and at most {MAX_URL} bytes (use punycode for IDN)"
            ));
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or("url must have a scheme (https://...)")?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            _ => {
                return Err(
                    "url must use https (http is allowed only for loopback IPv4 addresses)".into(),
                );
            }
        };
        if rest.contains('#') {
            return Err("url must not contain a fragment (#...)".into());
        }
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return Err(
                "url must not contain userinfo (user:pass@); configure auth.bearer instead".into(),
            );
        }
        let (host, port) = split_authority(authority)?;
        if host.contains(':') {
            return Err(
                "IPv6 literal addresses are not supported; use a hostname or IPv4 address".into(),
            );
        }
        let port = match port {
            Some(text) => text
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0 && text.bytes().all(|b| b.is_ascii_digit()))
                .ok_or("url port must be between 1 and 65535")?,
            None if tls => 443,
            None => 80,
        };
        let lower = host.to_ascii_lowercase();
        let host = if ends_in_number(&lower) {
            // Hosts parsed as IPv4 by URL parsers/OS must use canonical dotted-decimal form.
            let ip = lower
                .parse::<Ipv4Addr>()
                .ok()
                .filter(|ip| ip.to_string() == lower)
                .ok_or(
                    "url host uses a non-canonical numeric form; use canonical dotted-decimal IPv4 (e.g. 93.184.216.34)",
                )?;
            if !tls && !ip.is_loopback() {
                return Err(
                    "http is allowed only for loopback IPv4 addresses (127.0.0.0/8)".into(),
                );
            }
            ip.to_string()
        } else if !tls {
            return Err(
                "http is allowed only for loopback IPv4 addresses (127.0.0.0/8); use https for hostnames"
                    .into(),
            );
        } else {
            named(host)?
        };
        let path = match tail {
            "" => "/".to_owned(),
            query if query.starts_with('?') => format!("/{query}"),
            path => path.to_owned(),
        };
        Ok(Self {
            tls,
            host,
            port,
            path,
            bearer,
            oauth,
            oauth_issuer,
        })
    }

    pub fn auth_source(&self) -> Option<AuthSource> {
        if let Some(name) = &self.oauth {
            Some(AuthSource::Credential {
                name: name.clone(),
                expected_kind: CredentialKind::OAuth,
            })
        } else {
            self.bearer.clone().map(AuthSource::Secret)
        }
    }

    pub fn url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            format!("{scheme}://{}{}", self.host, self.path)
        } else {
            format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
        }
    }

    pub fn target(&self) -> HostPort {
        HostPort {
            host: self.host.clone(),
            port: self.port,
        }
    }

    /// `host:port` without path or query for diagnostics and allowlist messages.
    pub fn target_text(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Stable revision hash bound into approvals so scheme, endpoint, path, or credential changes invalidate prior approvals.
    pub fn revision(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"bot-server-v1-remote\0");
        let port = self.port.to_string();
        let bearer = self.bearer.as_ref().map_or("", SecretName::as_str);
        for part in [
            if self.tls { "https" } else { "http" },
            self.host.as_str(),
            port.as_str(),
            self.path.as_str(),
            bearer,
        ] {
            field(&mut hash, part.as_bytes());
        }
        // Append OAuth credential (and optional pinned issuer) after base fields so existing `None` and `Bearer` hashes remain byte-identical.
        if let Some(oauth) = &self.oauth {
            hash.update(b"oauth\0");
            field(&mut hash, oauth.as_str().as_bytes());
            if let Some(issuer) = &self.oauth_issuer {
                hash.update(b"issuer\0");
                field(&mut hash, issuer.as_bytes());
            }
        }
        format!("{:x}", hash.finalize())
    }
}

/// Splits `host[:port]` or `[IPv6][:port]`; returns IPv6 with `:` so the caller rejects it.
fn split_authority(authority: &str) -> Result<(&str, Option<&str>), String> {
    if let Some(inner) = authority.strip_prefix('[') {
        let (ip, after) = inner.split_once(']').ok_or("unclosed '[' in url host")?;
        return match after {
            "" => Ok((ip, None)),
            _ => after
                .strip_prefix(':')
                .map(|port| (ip, Some(port)))
                .ok_or_else(|| "only ':port' may follow url host".to_owned()),
        };
    }
    Ok(match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    })
}

/// WHATWG URL "ends in a number" check: a final non-empty label that is decimal or `0x`-prefixed hex is parsed as IPv4.
fn ends_in_number(host: &str) -> bool {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last() == Some(&"") {
        if parts.len() == 1 {
            return false;
        }
        parts.pop();
    }
    let last = parts.last().copied().unwrap_or_default();
    let hex = last
        .strip_prefix("0x")
        .or_else(|| last.strip_prefix("0X"))
        .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_hexdigit()));
    !last.is_empty() && (last.bytes().all(|b| b.is_ascii_digit()) || hex)
}

fn named(host: &str) -> Result<String, String> {
    canon_host(host)
        .filter(|name| {
            name.split('.')
                .all(|label| !label.starts_with('-') && !label.ends_with('-'))
        })
        .ok_or_else(|| {
            "url host is not a valid hostname (lowercase ASCII, digits, '.', '-'; labels cannot start or end with '-')"
                .to_owned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(url: &str) -> Remote {
        Remote::parse(url, None).unwrap_or_else(|why| panic!("{url}: {why}"))
    }

    fn refused(url: &str) -> String {
        Remote::parse(url, None).expect_err(url)
    }

    #[test]
    fn https_names_and_loopback_http_literals_are_accepted() {
        let r = ok("https://API.Example.com/mcp?v=2");
        assert_eq!(
            (r.tls, r.host.as_str(), r.port, r.path.as_str()),
            (true, "api.example.com", 443, "/mcp?v=2")
        );
        assert_eq!(ok("https://mcp.example:8443").path, "/");
        assert_eq!(ok("https://mcp.example?x=1").path, "/?x=1");
        assert_eq!(ok("HTTPS://mcp.example./mcp").host, "mcp.example");
        let local = ok("http://127.0.0.1:9000/mcp");
        assert_eq!(
            (local.tls, local.host.as_str(), local.port),
            (false, "127.0.0.1", 9000)
        );
        assert_eq!(ok("http://127.9.9.9/").port, 80);
        // HTTPS IP literals are parsed here and checked against public-IP egress rules at connect time.
        assert_eq!(ok("https://93.184.216.34/mcp").host, "93.184.216.34");
    }

    #[test]
    fn everything_else_is_refused() {
        for url in [
            "http://mcp.example/mcp",
            "http://localhost:9000/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://0.0.0.0/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::1]:7/m",
            "http://[::1]/",
            "https://[::1]/",
            "https://[2001:db8::1]:8443/mcp",
            "https://[::ffff:93.184.216.34]/",
            "http://2130706433/",
            "ftp://mcp.example/",
            "mcp.example/mcp",
            "https://user:pass@mcp.example/",
            "https://user@mcp.example/",
            "https://mcp.example/mcp#frag",
            "https://mcp.example:0/",
            "https://mcp.example:65536/",
            "https://mcp.example:44a/",
            "https://mcp.example:/",
            "https://127.1/",
            "https://2130706433/",
            "https://0x7f.1/",
            // Non-canonical numeric forms parsed as IPv4 by URL parsers/OS.
            "https://0x7f000001/",
            "https://0X7F000001/mcp",
            "https://0x7f.0x0.0x0.0x1/",
            "https://0177.0.0.1/",
            "https://0177.0.0.01/",
            "https://017700000001/",
            "https://127.0.1/",
            "https://127.0.0.1./mcp",
            "https://1.2.3.4./",
            "https://example.0x1/",
            "https://example.0/",
            "https://example.123/",
            "https://0x/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "https://a..b.example/",
            "https://-a.example/",
            "https://exämple.com/",
            "https://mcp.example/a b",
            "https://mcp.example/\n",
            "https://[::1/",
            "https://[::1]x/",
            "",
        ] {
            let why = refused(url);
            assert!(!why.is_empty(), "{url}");
        }
        assert!(refused(&format!("https://a.example/{}", "x".repeat(MAX_URL))).contains("2048"));
    }

    #[test]
    fn the_revision_binds_destination_scheme_path_and_credential_name() {
        let secret = |n: &str| SecretName::parse(n);
        let base = Remote::parse("https://mcp.example/mcp", secret("bearer")).unwrap();
        let same = Remote::parse("https://MCP.example:443/mcp", secret("bearer")).unwrap();
        assert_eq!(base.revision(), same.revision());
        for other in [
            Remote::parse("https://mcp.example/mcp", secret("other")),
            Remote::parse("https://mcp.example/mcp", None),
            Remote::parse("https://mcp.example/mcp2", secret("bearer")),
            Remote::parse("https://mcp.example:8443/mcp", secret("bearer")),
            Remote::parse("https://mcp2.example/mcp", secret("bearer")),
            Remote::parse("http://127.0.0.1/mcp", secret("bearer")),
        ] {
            assert_ne!(base.revision(), other.unwrap().revision());
        }
        assert_eq!(base.revision().len(), 64);
        assert_eq!(base.target_text(), "mcp.example:443");
        // Fixed golden hashes pinned before OAuth support so existing `None` and `Bearer` deployments never change.
        assert_eq!(
            Remote::parse("https://mcp.example/mcp", None)
                .unwrap()
                .revision(),
            "a807ff945a1c77445e8f303b06fdbe05a9bb5d77bbaf41e542b1971580b5923d"
        );
        assert_eq!(
            base.revision(),
            "e42ab6b1ea711d14c565d8d36b867a919372471f55b49dc5a61b815fcffc2faf"
        );
        let oauth =
            Remote::parse_oauth("https://mcp.example/mcp", secret("bearer").unwrap()).unwrap();
        assert_ne!(oauth.revision(), base.revision());
        assert_ne!(
            oauth.revision(),
            Remote::parse("https://mcp.example/mcp", None)
                .unwrap()
                .revision()
        );
        let oauth_pinned = Remote::parse_oauth_pinned(
            "https://mcp.example/mcp",
            secret("bearer").unwrap(),
            Some("https://auth.example/"),
        )
        .unwrap();
        assert_eq!(
            oauth_pinned.oauth_issuer.as_deref(),
            Some("https://auth.example")
        );
        assert_ne!(oauth_pinned.revision(), oauth.revision());
    }
}
