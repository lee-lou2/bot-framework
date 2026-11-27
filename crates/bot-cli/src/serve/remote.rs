//! Hardened reqwest-based `RemoteWire` transport for remote Streamable HTTP MCP servers.
use bot_broker::{
    egress::vet_resolved,
    mcp::wire::{RemoteWire, WireFail, WireMethod, WireReply, WireRequest, WireResponse},
    model::ChunkSource,
    proxy::Resolver,
};
use bot_core::port::{Error, Reply};
use reqwest::{
    Client,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

/// Must stay shorter than the broker handshake limit so pre-connect failures classify as `Unreached`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default idle read timeout; long tool calls supply their own total `WireRequest::timeout`.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolves `host` and returns one vetted IP only if every resolved address passes egress policy.
pub async fn vet_name(resolver: &dyn Resolver, host: &str, allow_private: bool) -> Option<IpAddr> {
    let found = resolver.resolve(host).await.ok()?;
    vet_resolved(&found, allow_private)
}

struct Vetted {
    resolver: Arc<dyn Resolver>,
    allow_private: bool,
}

impl Resolve for Vetted {
    fn resolve(&self, name: Name) -> Resolving {
        let (resolver, allow_private) = (Arc::clone(&self.resolver), self.allow_private);
        Box::pin(async move {
            match vet_name(resolver.as_ref(), name.as_str(), allow_private).await {
                Some(ip) => Ok(Box::new(std::iter::once(SocketAddr::new(ip, 0))) as Addrs),
                None => Err("blocked address".into()),
            }
        })
    }
}

/// Validates IP literals and hostnames: HTTPS requires public IPv4 (or private if explicitly allowed),
/// plain HTTP permits loopback IPv4 literals only, IPv6 literals are rejected, and hostnames require HTTPS.
pub fn literal_ok(host: &str, tls: bool, allow_private: bool) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if !tls => ip.is_loopback(),
        Ok(IpAddr::V4(ip)) => vet_resolved(&[IpAddr::V4(ip)], allow_private).is_some(),
        Ok(IpAddr::V6(_)) => false,
        Err(_) => tls,
    }
}

pub struct ReqwestWire {
    strict: Client,
    open: Client,
    /// Timed clients omit the idle `read_timeout` so per-request `WireRequest::timeout` governs long calls.
    strict_timed: Client,
    open_timed: Client,
}

impl ReqwestWire {
    pub fn new(resolver: Arc<dyn Resolver>) -> Result<Self, String> {
        Self::build(resolver, READ_TIMEOUT)
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn with_read_timeout(
        resolver: Arc<dyn Resolver>,
        read_timeout: Duration,
    ) -> Result<Self, String> {
        Self::build(resolver, read_timeout)
    }

    fn build(resolver: Arc<dyn Resolver>, read_timeout: Duration) -> Result<Self, String> {
        let make = |allow_private, timed: bool| {
            let builder = Client::builder().connect_timeout(CONNECT_TIMEOUT);
            let builder = if timed {
                builder
            } else {
                builder.read_timeout(read_timeout)
            };
            // Disable pooling, retries, redirects, and proxies so every connection is freshly vetted.
            builder
                .pool_max_idle_per_host(0)
                .retry(reqwest::retry::never())
                .redirect(Policy::none())
                .no_proxy()
                .dns_resolver(Arc::new(Vetted {
                    resolver: Arc::clone(&resolver),
                    allow_private,
                }))
                .build()
                .map_err(|e| {
                    format!(
                        "failed to build remote MCP HTTPS client (verify system CA certificates): {e}"
                    )
                })
        };
        Ok(Self {
            strict: make(false, false)?,
            open: make(true, false)?,
            strict_timed: make(false, true)?,
            open_timed: make(true, true)?,
        })
    }
}

struct Chunks(reqwest::Response);

impl ChunkSource for Chunks {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            match self.0.chunk().await {
                Ok(chunk) => Ok(chunk.map(|bytes| bytes.to_vec())),
                Err(_) => Err(Error::Unavailable),
            }
        })
    }
}

fn classify(error: &reqwest::Error) -> WireFail {
    let kind = if error.is_connect() || error.is_builder() {
        "connect"
    } else if error.is_timeout() {
        "timeout"
    } else {
        "request"
    };
    tracing::warn!(kind, "remote MCP request failed");
    if matches!(kind, "connect") {
        WireFail::Unreached
    } else {
        WireFail::Lost
    }
}

/// Ensures reqwest parses the exact scheme, host, and port that were vetted (blocks hex/octal/shorthand IP bypasses).
fn url_of(request: &WireRequest) -> Option<reqwest::Url> {
    let host = if request.host.contains(':') {
        format!("[{}]", request.host)
    } else {
        request.host.clone()
    };
    let scheme = if request.tls { "https" } else { "http" };
    let url = reqwest::Url::parse(&format!(
        "{scheme}://{host}:{}{}",
        request.port, request.path
    ))
    .ok()?;
    let same = url.scheme() == scheme
        && url.host_str() == Some(request.host.as_str())
        && url.port_or_known_default() == Some(request.port)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none();
    same.then_some(url)
}

impl RemoteWire for ReqwestWire {
    fn send(&self, request: WireRequest) -> WireReply<'_> {
        Box::pin(async move {
            // Vet literals before reqwest skips DNS resolution.
            if !literal_ok(&request.host, request.tls, request.allow_private) {
                return Err(WireFail::Unreached);
            }
            let mut headers = HeaderMap::new();
            for (name, value, secret) in &request.headers {
                let name =
                    HeaderName::from_bytes(name.as_bytes()).map_err(|_| WireFail::Unreached)?;
                let mut value = HeaderValue::from_str(value).map_err(|_| WireFail::Unreached)?;
                value.set_sensitive(*secret);
                headers.append(name, value);
            }
            let client = match (request.allow_private, request.timeout.is_some()) {
                (false, false) => &self.strict,
                (true, false) => &self.open,
                (false, true) => &self.strict_timed,
                (true, true) => &self.open_timed,
            };
            let Some(url) = url_of(&request) else {
                return Err(WireFail::Unreached);
            };
            let builder = match request.method {
                WireMethod::Post => client.post(url).body(request.body),
                WireMethod::Delete => client.delete(url),
            };
            let builder = match request.timeout {
                Some(limit) => builder.timeout(limit),
                None => builder,
            };
            let response = builder
                .headers(headers)
                .send()
                .await
                .map_err(|e| classify(&e))?;
            Ok(WireResponse {
                status: response.status().as_u16(),
                headers: response
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            String::from_utf8_lossy(value.as_bytes()).into_owned(),
                        )
                    })
                    .collect(),
                body: Box::new(Chunks(response)),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_broker::testing::FakeResolver;

    fn resolver() -> FakeResolver {
        FakeResolver::new(&[
            ("api.example", &["93.184.216.34"]),
            ("mixed.example", &["93.184.216.34", "10.0.0.7"]),
            ("rebind.example", &["10.0.0.7"]),
            ("loop.example", &["127.0.0.1"]),
            ("empty.example", &[]),
        ])
    }

    #[tokio::test]
    async fn only_names_whose_every_address_is_public_are_connected() {
        let r = resolver();
        assert_eq!(
            vet_name(&r, "api.example", false).await,
            Some("93.184.216.34".parse().unwrap())
        );
        for host in [
            "mixed.example",
            "rebind.example",
            "loop.example",
            "empty.example",
            "unknown.example",
        ] {
            assert_eq!(vet_name(&r, host, false).await, None, "{host}");
        }
        assert!(vet_name(&r, "rebind.example", true).await.is_some());
    }

    #[test]
    fn literals_are_vetted_and_only_a_loopback_literal_may_use_plain_http() {
        assert!(literal_ok("93.184.216.34", true, false));
        for host in [
            "10.0.0.7",
            "127.0.0.1",
            "169.254.169.254",
            "::1",
            "fd00::1",
            "0.0.0.0",
        ] {
            assert!(!literal_ok(host, true, false), "{host}");
        }
        assert!(literal_ok("10.0.0.7", true, true));
        assert!(literal_ok("127.0.0.1", false, false));
        assert!(literal_ok("127.9.9.9", false, false));
        for tls in [true, false] {
            assert!(!literal_ok("::1", tls, true), "tls={tls}");
            assert!(!literal_ok("2001:db8::1", tls, true), "tls={tls}");
        }
        for host in ["10.0.0.7", "93.184.216.34", "api.example", "localhost"] {
            assert!(!literal_ok(host, false, false), "{host}");
            assert!(!literal_ok(host, false, true), "allow_private {host}");
        }
        assert!(literal_ok("api.example", true, false));
    }

    #[test]
    fn the_url_the_parser_reads_must_be_the_one_that_was_judged() {
        let request = |host: &str, tls: bool| WireRequest {
            method: WireMethod::Post,
            tls,
            host: host.into(),
            port: 8443,
            path: "/mcp?a=1".into(),
            headers: Vec::new(),
            body: Vec::new(),
            allow_private: false,
            timeout: None,
        };
        let same = url_of(&request("api.example", true)).unwrap();
        assert_eq!(same.as_str(), "https://api.example:8443/mcp?a=1");
        assert!(url_of(&request("93.184.216.34", true)).is_some());
        for host in [
            "0x7f000001",
            "2130706433",
            "0177.0.0.1",
            "127.1",
            "127.0.0.1.",
            "::1",
            "API.example",
        ] {
            assert!(url_of(&request(host, true)).is_none(), "{host}");
        }
        let mut sneaky = request("api.example", true);
        sneaky.path = "@evil.example/x".into();
        assert!(url_of(&sneaky).is_none());
    }
}
