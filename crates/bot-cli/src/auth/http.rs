//! Hardened OAuth HTTP client enforcing fresh DNS vetting, HTTPS-only egress, and no redirects or retries.

use crate::serve::remote::{literal_ok, vet_name};
use bot_broker::{
    egress::EgressPolicy,
    mcp::remote::Remote,
    proxy::{Resolver, SystemResolver},
    resolve::HostPort,
};
use reqwest::{
    Client,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{ACCEPT, CONTENT_TYPE},
    redirect::Policy,
};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc, time::Duration};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 256 * 1024;

struct VettedDns {
    resolver: Arc<dyn Resolver>,
}

impl Resolve for VettedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = Arc::clone(&self.resolver);
        Box::pin(async move {
            match vet_name(resolver.as_ref(), name.as_str(), false).await {
                Some(ip) => Ok(Box::new(std::iter::once(SocketAddr::new(ip, 0))) as Addrs),
                None => Err("blocked address".into()),
            }
        })
    }
}

/// Hardened HTTP client used for OAuth discovery, registration, token exchange, and refresh.
#[derive(Clone)]
pub struct AuthHttp {
    client: Client,
    allow_loopback_http: bool,
}

impl AuthHttp {
    pub fn new() -> Result<Self, String> {
        Self::with_resolver(Arc::new(SystemResolver), false)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn for_testing(resolver: Arc<dyn Resolver>) -> Result<Self, String> {
        Self::with_resolver(resolver, true)
    }

    fn with_resolver(
        resolver: Arc<dyn Resolver>,
        allow_loopback_http: bool,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(VettedDns { resolver }))
            .build()
            .map_err(|e| format!("failed to build OAuth HTTP client: {e}"))?;
        Ok(Self {
            client,
            allow_loopback_http,
        })
    }

    pub fn allow_loopback_http(&self) -> bool {
        self.allow_loopback_http
    }

    /// Validates scheme, IP literal safety, and required profile egress cap for any OAuth URL (loopback test mode may omit cap only for `127.0.0.1`).
    pub fn vet_url(
        &self,
        url: &str,
        profile_egress: Option<&[HostPort]>,
    ) -> Result<(Remote, reqwest::Url), String> {
        let remote = Remote::parse(url, None)?;
        if !remote.tls && !self.allow_loopback_http {
            return Err("OAuth endpoints require https".to_owned());
        }
        if !literal_ok(&remote.host, remote.tls, false) {
            return Err("OAuth endpoint address is disallowed by egress policy".to_owned());
        }
        let test_loopback = self.allow_loopback_http && remote.host == "127.0.0.1";
        match profile_egress {
            Some(cap) if !cap.is_empty() => {
                if !EgressPolicy::new(cap, Some(cap)).permits(&remote.host, remote.port) {
                    return Err(format!(
                        "OAuth endpoint `{}` is not permitted by profile egress cap",
                        remote.target_text()
                    ));
                }
            }
            _ if test_loopback => {}
            _ => {
                return Err(format!(
                    "OAuth endpoint `{}` requires an explicit `egress.<profile>` allowlist entry",
                    remote.target_text()
                ));
            }
        }
        let parsed = reqwest::Url::parse(&remote.url())
            .map_err(|_| "invalid OAuth endpoint URL".to_owned())?;
        let scheme = if remote.tls { "https" } else { "http" };
        let same = parsed.scheme() == scheme
            && parsed.host_str() == Some(remote.host.as_str())
            && parsed.port_or_known_default() == Some(remote.port)
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none();
        if !same {
            return Err("OAuth endpoint URL failed canonical check".to_owned());
        }
        Ok((remote, parsed))
    }

    pub async fn get_json(
        &self,
        url: &str,
        profile_egress: Option<&[HostPort]>,
    ) -> Result<(u16, Value), String> {
        let (_, parsed) = self.vet_url(url, profile_egress)?;
        let resp = self
            .client
            .get(parsed)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| "OAuth HTTP GET failed".to_owned())?;
        read_json(resp).await
    }

    pub async fn post_form(
        &self,
        url: &str,
        pairs: &[(&str, &str)],
        profile_egress: Option<&[HostPort]>,
    ) -> Result<(u16, Value), String> {
        let (_, parsed) = self.vet_url(url, profile_egress)?;
        let body = form_encode(pairs);
        let resp = self
            .client
            .post(parsed)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| "OAuth HTTP POST failed".to_owned())?;
        read_json(resp).await
    }

    pub async fn post_json(
        &self,
        url: &str,
        body: &Value,
        profile_egress: Option<&[HostPort]>,
    ) -> Result<(u16, Value), String> {
        let (_, parsed) = self.vet_url(url, profile_egress)?;
        let resp = self
            .client
            .post(parsed)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| "OAuth HTTP POST failed".to_owned())?;
        read_json(resp).await
    }

    /// Sends an unauthenticated probe to read `WWW-Authenticate` on a remote MCP endpoint.
    pub async fn probe_www_authenticate(
        &self,
        url: &str,
        profile_egress: Option<&[HostPort]>,
    ) -> Result<Option<String>, String> {
        let (_, parsed) = self.vet_url(url, profile_egress)?;
        let resp = self
            .client
            .post(parsed)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .send()
            .await
            .map_err(|_| "failed to probe remote MCP server".to_owned())?;
        Ok(resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned))
    }
}

async fn read_json(mut resp: reqwest::Response) -> Result<(u16, Value), String> {
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| "OAuth response read failed".to_owned())?
    {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err("OAuth response exceeded size limit".to_owned());
        }
        bytes.extend_from_slice(&chunk);
    }
    let parsed = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    Ok((status, parsed))
}

/// RFC 3986 percent-encoding for `application/x-www-form-urlencoded` bodies and query params.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_broker::testing::FakeResolver;

    #[test]
    fn vets_https_and_profile_egress_cap() {
        let http = AuthHttp::new().unwrap();
        let cap = vec![HostPort::parse("auth.example:443").unwrap()];
        assert!(
            http.vet_url("https://auth.example/oauth/token", Some(&cap))
                .is_ok()
        );
        assert!(
            http.vet_url("https://other.example/oauth/token", Some(&cap))
                .is_err()
        );
        assert!(
            http.vet_url("https://auth.example/oauth/token", None)
                .is_err()
        );
        assert!(
            http.vet_url("https://auth.example/oauth/token", Some(&[]))
                .is_err()
        );
        assert!(http.vet_url("http://127.0.0.1:8080/token", None).is_err());
        assert!(http.vet_url("https://10.0.0.1/token", None).is_err());
        assert!(
            http.vet_url("https://169.254.169.254/latest", None)
                .is_err()
        );

        let test_http = AuthHttp::for_testing(Arc::new(FakeResolver::new(&[]))).unwrap();
        assert!(
            test_http
                .vet_url("http://127.0.0.1:8080/token", None)
                .is_ok()
        );
        assert!(
            test_http
                .vet_url("http://auth.example/token", None)
                .is_err()
        );
    }

    #[test]
    fn form_encodes_reserved_bytes() {
        assert_eq!(
            form_encode(&[("grant_type", "refresh_token"), ("scope", "a b+c")]),
            "grant_type=refresh_token&scope=a%20b%2Bc"
        );
    }
}
