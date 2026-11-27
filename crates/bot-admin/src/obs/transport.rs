//! HTTP/HTTPS POST transport for observability exporters with localhost loopback pinning and fixed error codes.
//! Plain `http://` uses raw `TcpStream`; `https://` uses `reqwest` without redirects, retries, or connection pooling.

use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    Http,
    Https,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub path: String,
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
}

fn is_localhost_host(host: &str) -> bool {
    let name = host.to_ascii_lowercase();
    name == "localhost" || name.ends_with(".localhost")
}

fn loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => local_ip(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        },
    }
}

impl Target {
    pub(crate) fn parse_with_default(url: &str, default_path: &str) -> Result<Self, &'static str> {
        let (scheme, default_port, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, 443, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, 80, rest)
        } else {
            return Err("only http:// and https:// URLs are supported");
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, default_path),
        };
        if path.contains(['?', '#', ' '])
            || path.bytes().any(|b| b < 0x20 || b == 0x7f)
            || authority.contains('@')
        {
            return Err("URL must not contain userinfo, query, fragment, or control characters");
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, tail) = v6.split_once(']').ok_or("invalid IPv6 address")?;
            if h.parse::<Ipv6Addr>().is_err() {
                return Err("invalid IPv6 address");
            }
            let port = if tail.is_empty() {
                None
            } else if let Some(p) = tail.strip_prefix(':') {
                Some(p)
            } else {
                return Err("invalid IPv6 address");
            };
            (h, port)
        } else {
            let (h, port) = match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            };
            if !host_ok(h) {
                return Err("invalid host");
            }
            (h, port)
        };
        let port = match port {
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or("invalid port")?,
            None => default_port,
        };
        Ok(Self {
            scheme,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    pub(crate) fn stays_local(&self) -> bool {
        match self.host.parse::<IpAddr>() {
            Ok(ip) => local_ip(ip),
            Err(_) => is_localhost_host(&self.host),
        }
    }

    pub(crate) fn is_loopback(&self) -> bool {
        match self.host.parse::<IpAddr>() {
            Ok(ip) => loopback_ip(ip),
            Err(_) => is_localhost_host(&self.host),
        }
    }

    /// Pins `localhost` and `*.localhost` to loopback addresses without consulting the OS resolver.
    pub(crate) fn dial(&self) -> Option<Vec<SocketAddr>> {
        match self.host.parse::<IpAddr>() {
            Ok(ip) => Some(vec![SocketAddr::new(ip, self.port)]),
            Err(_) if is_localhost_host(&self.host) => Some(vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), self.port),
            ]),
            Err(_) => None,
        }
    }

    pub(crate) fn origin(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub(crate) fn url(&self) -> String {
        let prefix = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        format!("{prefix}://{}{}", self.origin(), self.path)
    }
}

/// Transport-owned headers rejected from user config to prevent duplication and request smuggling.
pub(crate) fn is_reserved_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "content-type" | "transfer-encoding" | "connection"
    )
}

pub(crate) fn header_ok(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !is_reserved_header(name)
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && value.len() <= 4096
        && value
            .bytes()
            .all(|c| c == b'\t' || (0x20..0x7f).contains(&c))
}

/// DNS resolver for `https://` that pins `localhost` and `*.localhost` to loopback without OS resolution.
struct LoopbackPinResolver;

impl Resolve for LoopbackPinResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            if is_localhost_host(&host) {
                let addrs = [
                    SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
                    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0),
                ];
                return Ok(Box::new(addrs.into_iter()) as Addrs);
            }
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            Ok(Box::new(resolved.into_iter()) as Addrs)
        })
    }
}

#[derive(Clone)]
struct HttpsPair {
    remote: reqwest::Client,
    local: reqwest::Client,
}

#[derive(Clone)]
pub(crate) struct Client {
    https: Option<HttpsPair>,
}

impl Client {
    pub(crate) fn for_targets<'a>(
        targets: impl IntoIterator<Item = &'a Target>,
    ) -> Result<Self, String> {
        if targets.into_iter().any(|t| t.scheme == Scheme::Https) {
            Self::build(None)
        } else {
            Ok(Self { https: None })
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_root_pem(pem: &[u8]) -> Result<Self, String> {
        let cert = reqwest::Certificate::from_pem(pem)
            .map_err(|_| "failed to parse test TLS certificate".to_owned())?;
        Self::build(Some(cert))
    }

    fn build(extra_root: Option<reqwest::Certificate>) -> Result<Self, String> {
        let make = |no_proxy: bool| {
            let mut builder = reqwest::Client::builder()
                .redirect(Policy::none())
                .retry(reqwest::retry::never())
                .pool_max_idle_per_host(0)
                .user_agent(concat!("bot/", env!("CARGO_PKG_VERSION")))
                .dns_resolver(Arc::new(LoopbackPinResolver));
            if no_proxy {
                builder = builder.no_proxy();
            }
            if let Some(cert) = extra_root.clone() {
                builder = builder.add_root_certificate(cert);
            }
            builder.build().map_err(|_| {
                "failed to initialize observability HTTPS client (check CA certificates)".to_owned()
            })
        };
        Ok(Self {
            https: Some(HttpsPair {
                remote: make(false)?,
                local: make(true)?,
            }),
        })
    }

    pub(crate) async fn post(
        &self,
        t: &Target,
        content_type: &str,
        headers: &[(String, String)],
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), &'static str> {
        match t.scheme {
            Scheme::Http => post_plain_http(t, content_type, headers, body, timeout).await,
            Scheme::Https => {
                self.post_https(t, content_type, headers, body, timeout)
                    .await
            }
        }
    }

    async fn post_https(
        &self,
        t: &Target,
        content_type: &str,
        headers: &[(String, String)],
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), &'static str> {
        let Some(pair) = &self.https else {
            return Err("config");
        };
        let mut map = HeaderMap::new();
        let ct = HeaderValue::from_str(content_type).map_err(|_| "config")?;
        map.insert(CONTENT_TYPE, ct);
        for (k, v) in headers {
            if !header_ok(k, v) {
                return Err("config");
            }
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| "config")?;
            let mut val = HeaderValue::from_str(v).map_err(|_| "config")?;
            val.set_sensitive(true);
            map.append(name, val);
        }
        let client = if t.stays_local() {
            &pair.local
        } else {
            &pair.remote
        };
        let req = client
            .post(t.url())
            .headers(map)
            .timeout(timeout)
            .body(body.to_vec());
        match tokio::time::timeout(timeout, req.send()).await {
            Ok(Ok(resp)) if resp.status().is_success() => Ok(()),
            Ok(Ok(_)) => Err("status"),
            Ok(Err(err)) if err.is_timeout() => Err("timeout"),
            Ok(Err(err)) if err.is_connect() => Err("connect"),
            Ok(Err(_)) => Err("io"),
            Err(_) => Err("timeout"),
        }
    }
}

async fn post_plain_http(
    t: &Target,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<(), &'static str> {
    for (k, v) in headers {
        if !header_ok(k, v) {
            return Err("config");
        }
    }
    tokio::time::timeout(timeout, async {
        let mut s = match t.dial() {
            Some(addrs) => TcpStream::connect(&addrs[..]).await,
            None => TcpStream::connect((t.host.as_str(), t.port)).await,
        }
        .map_err(|_| "connect")?;
        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            t.path,
            t.origin(),
            content_type,
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).await.map_err(|_| "io")?;
        s.write_all(body).await.map_err(|_| "io")?;
        let (mut buf, mut chunk) = (Vec::new(), [0u8; 512]);
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 4096 {
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err("io"),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        match parse_status_line(&buf) {
            Some(200..=299) => Ok(()),
            Some(_) => Err("status"),
            None => Err("io"),
        }
    })
    .await
    .unwrap_or(Err("timeout"))
}

/// Parses an RFC 9112 status line (`HTTP/1.x <code>[ reason]\r\n`), allowing an empty reason phrase.
fn parse_status_line(buf: &[u8]) -> Option<u16> {
    let end = buf.windows(2).position(|w| w == b"\r\n")?;
    let line = String::from_utf8_lossy(&buf[..end]);
    if !line.starts_with("HTTP/1.") {
        return None;
    }
    let code = line.split_ascii_whitespace().nth(1)?;
    (code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()))
        .then(|| code.parse::<u16>().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Target {
        Target::parse_with_default(url, "/v1/traces").expect("valid")
    }

    fn addrs(list: &[&str]) -> Option<Vec<SocketAddr>> {
        Some(list.iter().map(|a| a.parse().expect("addr")).collect())
    }

    #[test]
    fn http_and_https_endpoints_are_accepted_and_bad_shapes_are_refused() {
        let t = target("http://127.0.0.1:4318/v1/traces");
        assert_eq!(
            (t.scheme, t.host.as_str(), t.port, t.path.as_str()),
            (Scheme::Http, "127.0.0.1", 4318, "/v1/traces")
        );
        let tls = target("https://cloud.langfuse.com/api/public/otel/v1/traces");
        assert_eq!(
            (tls.scheme, tls.host.as_str(), tls.port, tls.path.as_str()),
            (
                Scheme::Https,
                "cloud.langfuse.com",
                443,
                "/api/public/otel/v1/traces"
            )
        );
        assert_eq!(target("https://api.smith.langchain.com").path, "/v1/traces");
        assert_eq!(target("http://collector.internal").port, 80);
        let v6 = target("http://[::1]:4318/x");
        assert_eq!(
            (v6.host.as_str(), v6.origin().as_str()),
            ("::1", "[::1]:4318")
        );
        let v6_default = target("https://[::1]/v1/traces");
        assert_eq!((v6_default.host.as_str(), v6_default.port), ("::1", 443));
        for bad in [
            "ftp://x",
            "collector:4318",
            "http://",
            "https://",
            "http://user:pw@collector:4318",
            "https://key@o0.ingest.sentry.io/1",
            "http://collector:0",
            "http://collector:99999",
            "http://collector:4318/v1/traces?x=1",
            "https://collector:4318/v1/traces#frag",
            "http://col lector:4318",
            "http://[::1",
            "https://[::1]garbage/v1/traces",
            "http://a:b:4318/v1",
            "https://[not-ipv6]:443/v1",
            "http://[::1]:/v1",
        ] {
            assert!(
                Target::parse_with_default(bad, "/v1/traces").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn localhost_names_are_dialed_at_loopback_without_asking_the_resolver() {
        for url in [
            "http://localhost:4318/v1/traces",
            "http://LOCALHOST:4318",
            "http://collector.localhost:4318",
            "https://a.b.c.LocalHost:4318",
        ] {
            let t = target(url);
            assert!(t.stays_local() && t.is_loopback(), "{url}");
            assert_eq!(
                t.dial(),
                addrs(&["127.0.0.1:4318", "[::1]:4318"]),
                "{url}: pinned to loopback (IPv4 first)"
            );
        }
        let t = target("http://Collector.localhost:4318/x");
        assert_eq!(t.origin(), "Collector.localhost:4318");
    }

    #[test]
    fn ip_literals_are_dialed_exactly_as_written_and_other_names_are_left_to_the_resolver() {
        for (url, expected) in [
            ("http://127.0.0.1:4318", "127.0.0.1:4318"),
            ("http://10.1.2.3:80", "10.1.2.3:80"),
            ("http://203.0.113.7:9", "203.0.113.7:9"),
            ("http://[::1]:4318", "[::1]:4318"),
            ("http://[fd00::1]:4318", "[fd00::1]:4318"),
        ] {
            assert_eq!(target(url).dial(), addrs(&[expected]), "{url}");
        }
        for url in [
            "http://collector.example.com:4318",
            "http://otel-collector:4318",
            "http://127.0.0.1.example.com:4318",
            "http://notlocalhost:4318",
            "http://localhost.example.com:4318",
            "http://2130706433:4318",
        ] {
            let t = target(url);
            assert_eq!(t.dial(), None, "{url}");
            assert!(!t.stays_local(), "{url}");
        }
    }

    #[test]
    fn header_names_and_values_cannot_smuggle_lines() {
        assert!(header_ok("Authorization", "Bearer abc"));
        assert!(header_ok("x-api-key", "k"));
        for (n, v) in [
            ("", "v"),
            ("a b", "v"),
            ("a:b", "v"),
            ("a", "v\r\nX: y"),
            ("a", "v\n"),
            ("a", "v\0"),
            ("a", "non-ASCII-€"),
            ("Host", "evil.example"),
            ("Content-Type", "text/plain"),
            ("Content-Length", "0"),
            ("Transfer-Encoding", "chunked"),
            ("Connection", "keep-alive"),
        ] {
            assert!(!header_ok(n, v), "{n:?}");
        }
    }

    #[tokio::test]
    async fn plain_http_accepts_status_line_without_reason_phrase_and_rejects_non_http() {
        use tokio::net::TcpListener;

        async fn send(raw_reply: &'static [u8]) -> Result<(), &'static str> {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                if let Ok((mut s, _)) = listener.accept().await {
                    let mut req = [0u8; 1024];
                    let _ = s.read(&mut req).await;
                    let _ = s.write_all(raw_reply).await;
                }
            });
            let t = Target::parse_with_default(&format!("http://{addr}/v1/traces"), "/v1/traces")
                .unwrap();
            post_plain_http(&t, "application/json", &[], b"{}", Duration::from_secs(5)).await
        }

        assert_eq!(
            send(b"HTTP/1.1 200\r\nContent-Length: 0\r\n\r\n").await,
            Ok(())
        );
        assert_eq!(
            send(b"HTTP/1.1 204 \r\nContent-Length: 0\r\n\r\n").await,
            Ok(())
        );
        assert_eq!(
            send(b"HTTP/1.1 503\r\nContent-Length: 0\r\n\r\n").await,
            Err("status")
        );
        assert_eq!(send(b"GARBAGE 200 OK\r\n\r\n").await, Err("io"));
        assert_eq!(send(b"HTTP/2 200\r\n\r\n").await, Err("io"));
    }
}
