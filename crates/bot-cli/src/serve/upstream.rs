//! 모델 상류(HTTPS)로 나가는 운영 전송(`bot_broker::model::Upstream`). 프런트의 `ModelProxy`가 요청을 검사하고 서비스 키까지 실은 뒤 이 전송이 보낸다.
//!
//! - TLS는 rustls와 시스템 루트 인증서다. 목적지는 호스트 설정의 `providers.<별칭>.endpoint`가 정하고 모델 CLI는 바꾸지 못한다.
//! - **숨은 재전송이 없다**: 재시도·리다이렉트·연결 재사용을 모두 끈다. 못 보냈거나 응답을 못 받았으면 `Unavailable`이고 다시 보낼지는 위(프런트·모델 CLI)가 정한다.
//! - 일반 응답은 `MAX_BODY`까지만 읽는다(넘으면 `Unavailable`). 스트림은 조각을 그대로 내주고 크기·시간 한도는 프록시가 정한다.
//! - 머리 값(서비스 키)은 `sensitive`로 표시해 어떤 로그에도 값이 나가지 않게 한다. 실패는 분류만 남기고 요청·응답 내용은 남기지 않는다.
use bot_broker::model::{ChunkSource, Upstream, UpstreamRequest, UpstreamResponse, UpstreamStream};
use bot_core::port::{Error, Reply};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::time::Duration;

/// 연결을 맺는 시간
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 응답 머리·본문 조각 하나를 기다리는 시간. 프록시의 스트림 유휴(60초)·전체 시간 한도가 더 엄격하다
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// 일반 응답 본문의 읽기 상한(프록시의 한도 8 MiB보다 넉넉하게 — 넘기면 프록시가 거절한다)
const MAX_BODY: usize = 16 << 20;

pub struct HttpsUpstream {
    client: Client,
    scheme: &'static str,
    max_body: usize,
}

impl HttpsUpstream {
    /// 운영 전송: `https`만 쓴다.
    pub fn new() -> Result<Self, String> {
        Self::build("https", MAX_BODY)
    }

    /// 시험 전용: 로컬 가짜 서버(`http`)로 보낸다. 운영 조립은 이 길을 쓰지 않는다.
    #[doc(hidden)]
    pub fn over_http(max_body: usize) -> Result<Self, String> {
        Self::build("http", max_body)
    }

    fn build(scheme: &'static str, max_body: usize) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            // 닫힌 유휴 연결로 보냈다가 클라이언트가 몰래 다시 보내는 일을 막는다
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(Policy::none())
            .build()
            .map_err(|e| {
                format!("상류 HTTPS 클라이언트를 만들지 못했어요(CA 인증서가 있는지 확인): {e}")
            })?;
        Ok(Self {
            client,
            scheme,
            max_body,
        })
    }

    /// 요청 주소. IPv6 리터럴은 대괄호로 감싼다.
    fn url(&self, request: &UpstreamRequest) -> String {
        let host = &request.endpoint.host;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        format!(
            "{}://{host}:{}{}",
            self.scheme, request.endpoint.port, request.path
        )
    }

    /// 보낼 요청. 머리 이름·값이 올바르지 않으면 연결하기 전에 `Invalid`다.
    fn request(&self, request: UpstreamRequest) -> Result<reqwest::RequestBuilder, Error> {
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Invalid)?;
            let mut value = HeaderValue::from_str(value).map_err(|_| Error::Invalid)?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
        Ok(self
            .client
            .post(self.url(&request))
            .headers(headers)
            .body(request.body))
    }
}

/// 실패를 값 없이 분류만 남긴다.
fn unavailable(error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() || error.is_decode() {
        "body"
    } else {
        "request"
    };
    tracing::warn!(kind, "모델 상류 요청이 실패했어요");
    Error::Unavailable
}

impl Upstream for HttpsUpstream {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse> {
        Box::pin(async move {
            let mut response = self
                .request(request)?
                .send()
                .await
                .map_err(|e| unavailable(&e))?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| unavailable(&e))? {
                if body.len() + chunk.len() > self.max_body {
                    tracing::warn!("모델 상류 응답이 한도를 넘어 받지 않아요");
                    return Err(Error::Unavailable);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(UpstreamResponse { status, body })
        })
    }

    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream> {
        Box::pin(async move {
            let response = self
                .request(request)?
                .send()
                .await
                .map_err(|e| unavailable(&e))?;
            Ok(UpstreamStream {
                status: response.status().as_u16(),
                body: Box::new(Chunks(response)),
            })
        })
    }
}

/// 응답 본문을 도착하는 대로 내주는 원천
struct Chunks(reqwest::Response);

impl ChunkSource for Chunks {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            self.0
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|e| unavailable(&e))
        })
    }
}

#[cfg(test)]
mod tests;
