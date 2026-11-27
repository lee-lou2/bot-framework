//! 원격 MCP 서버로 나가는 운영 전송(`bot_broker::mcp::wire::RemoteWire`). 브로커의 `McpRunner`와 `bot mcp sync`가 같은 전송을 쓴다.
//!
//! - TLS는 rustls와 시스템 루트 인증서다. 접속 대상은 호스트 설정의 서버 주소가 정하고 서버가 바꾸지 못한다.
//! - **연결마다** 이름 풀이 결과 **전체**를 검사하고(`egress::vet_resolved`: 하나라도 비공개 주소면 그 이름 전체를 거절) 검사한 주소에만 연결한다
//!   (리졸버가 검사한 주소 하나만 돌려주고, 연결을 재사용하지 않는다). IPv4 리터럴 호스트는 이름 풀이를 거치지 않으므로 같은 검사를 여기서 직접 한다.
//!   예외는 `https`가 아닌 **루프백 IPv4 리터럴(127.0.0.0/8)** 하나뿐이다(전역 `allow_private`로 넓히지 않는다). IPv6 리터럴은 쓰지 않는다.
//! - **판정한 호스트가 곧 연결하는 호스트다**: 요청을 `reqwest`가 쓰는 URL 파서로 한 번 풀어 호스트·포트·방식이 판정한 값과 글자 그대로 같을 때만 보낸다
//!   (파서가 `0x7f000001`·`2130706433`·`127.1`을 127.0.0.1로 읽어 이름 풀이 검사를 건너뛰는 틈을 연결 전에 닫는다).
//! - **긴 호출은 자기 제한을 가진다**: 요청이 `timeout`을 실으면(`tools/call`, 호출 한도 + 여유) 응답 머리·본문 조각 사이의 무응답 읽기 제한(120초) 대신 그 요청 전체의 제한을 쓴다 —
//!   서버가 답을 늦게 주는 긴 비스트림 응답이 기본 제한에 먼저 끊기지 않는다. 연결을 맺기 전의 제한(이름 풀이·TCP·TLS, 5초)은 두 경우 모두 그대로다.
//! - **숨은 재전송이 없다**: 리다이렉트(3xx는 상태 그대로)·재시도·연결 재사용·프록시·압축 해제를 모두 끈다.
//! - 비밀 머리(`Authorization`)는 `sensitive`로 표시해 어떤 로그에도 값이 나가지 않게 한다. 실패는 분류만 남긴다.
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

/// 연결을 맺는 시간(브로커의 협상 한도보다 짧아야 «연결 전 실패»가 시간 초과로 가려지지 않는다)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 응답 머리·본문 조각 하나를 기다리는 기본 시간. 호출 전체 시간은 브로커가 정하고, 긴 호출은 요청이 자기 제한(`WireRequest::timeout`)을 싣는다.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// 이름 하나를 풀어 전체를 검사하고 검사한 주소 하나만 돌려준다. 비어 있거나 비공개 주소가 하나라도 있으면 `None`.
pub async fn vet_name(resolver: &dyn Resolver, host: &str, allow_private: bool) -> Option<IpAddr> {
    let found = resolver.resolve(host).await.ok()?;
    vet_resolved(&found, allow_private)
}

/// 이름 풀이를 검사하는 `reqwest` 리졸버. 연결은 돌려준 주소(포트는 URL이 정한다)로만 간다.
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

/// IP 리터럴 호스트: `https`는 공개 IPv4 주소여야 하고(`allow_private`가 열렸으면 사설 주소도), `https`가 아닌 것은 루프백 IPv4(127.0.0.0/8)여야 한다.
/// IPv6 리터럴은 쓰지 않는다. 이름은 `https`만 된다.
pub fn literal_ok(host: &str, tls: bool, allow_private: bool) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if !tls => ip.is_loopback(),
        Ok(IpAddr::V4(ip)) => vet_resolved(&[IpAddr::V4(ip)], allow_private).is_some(),
        Ok(IpAddr::V6(_)) => false,
        Err(_) => tls,
    }
}

pub struct ReqwestWire {
    /// 사설망을 열지 않은 클라이언트(기본)
    strict: Client,
    /// 운영자가 사설망을 명시적으로 연 클라이언트
    open: Client,
    /// 요청이 자기 제한을 싣는 경우의 클라이언트들(무응답 읽기 제한이 없고 요청마다 전체 제한을 건다. 연결 제한은 같다)
    strict_timed: Client,
    open_timed: Client,
}

impl ReqwestWire {
    pub fn new(resolver: Arc<dyn Resolver>) -> Result<Self, String> {
        Self::build(resolver, READ_TIMEOUT)
    }

    /// 기본 무응답 읽기 제한을 정해 만든다(시험이 짧은 값으로 «긴 호출은 기본 제한에 끊기지 않는다»를 확인하는 데 쓴다). 운영 빌드에는 포함하지 않는다.
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
                        "원격 MCP HTTPS 클라이언트를 만들지 못했어요(CA 인증서가 있는지 확인): {e}"
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

/// 응답 본문 조각을 내주는 원천.
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

/// 연결을 맺기 전의 실패(이름 풀이·주소 검사·TCP·TLS)면 요청이 서버에 닿지 않았다. 그 밖은 서버가 받았는지 모른다.
fn classify(error: &reqwest::Error) -> WireFail {
    let kind = if error.is_connect() || error.is_builder() {
        "connect"
    } else if error.is_timeout() {
        "timeout"
    } else {
        "request"
    };
    tracing::warn!(kind, "원격 MCP 요청이 실패했어요");
    if matches!(kind, "connect") {
        WireFail::Unreached
    } else {
        WireFail::Lost
    }
}

/// 요청을 `reqwest`가 쓰는 URL 파서로 풀되, 호스트·포트·방식이 이쪽이 판정한 값과 글자 그대로 같을 때만 돌려준다(그렇지 않으면 파서가 다른 곳으로 읽은 것이다).
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
            // 이름이든 리터럴이든 연결 전에 거른다(리터럴은 리졸버를 거치지 않는다)
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
        // 하나라도 비공개면 그 이름 전체를 거절한다(DNS 재바인딩)
        for host in [
            "mixed.example",
            "rebind.example",
            "loop.example",
            "empty.example",
            "unknown.example",
        ] {
            assert_eq!(vet_name(&r, host, false).await, None, "{host}");
        }
        // 사설망을 연 운영자만 사설 주소로 간다
        assert!(vet_name(&r, "rebind.example", true).await.is_some());
    }

    #[test]
    fn literals_are_vetted_and_only_a_loopback_literal_may_use_plain_http() {
        // https: 공개 주소만(사설망을 열면 사설도)
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
        // http: 루프백 리터럴만. 전역 allow_private로 넓어지지 않는다
        assert!(literal_ok("127.0.0.1", false, false));
        assert!(literal_ok("127.9.9.9", false, false));
        // IPv6 리터럴은 어느 방식에서도 쓰지 않는다
        for tls in [true, false] {
            assert!(!literal_ok("::1", tls, true), "tls={tls}");
            assert!(!literal_ok("2001:db8::1", tls, true), "tls={tls}");
        }
        for host in ["10.0.0.7", "93.184.216.34", "api.example", "localhost"] {
            assert!(!literal_ok(host, false, false), "{host}");
            assert!(!literal_ok(host, false, true), "allow_private {host}");
        }
        // 이름은 https만
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
        // 파서가 다른 호스트로 읽는 표기·IPv6·대문자(정규화 전)는 판정한 값과 달라 보내지 않는다
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
        // 경로에 사용자 정보·조각을 숨길 수 없다
        let mut sneaky = request("api.example", true);
        sneaky.path = "@evil.example/x".into();
        assert!(url_of(&sneaky).is_none());
    }
}
