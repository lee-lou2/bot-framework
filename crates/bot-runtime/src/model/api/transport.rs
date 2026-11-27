//! 상류로 나가는 POST 전송. `serve/upstream.rs`의 원칙 그대로다: 리다이렉트·숨은 재시도·연결 재사용을 모두 끄고, 연결 시간 한도와 본문 크기 상한을 둔다. 못 보냈거나 응답을 못 받았으면 실패로
//! 분류만 하고 다시 보낼지는 위(워커 정책)가 정한다. 머리 값(서비스 키)은 `sensitive`로 표시해 어떤 진단 출력에도 값이 나가지 않게 한다. 실패는 분류만 남기고 요청·응답 내용은 남기지 않는다.
//!
//! 응답 본문은 2xx일 때만 끝까지 읽는다. 2xx가 아닌 오류 응답 본문은 분류(`insufficient_quota`·허용 목록 오류 종류)에만 쓰려고 작은 상한(`ERROR_BODY_MAX`) 안에서만 읽고,
//! 상한을 넘는 나머지는 읽지 않고 연결을 닫는다. 원문은 어디에도 남기지 않는다. 전체 시간 한도(`Limits::total`)와 취소는 부르는 쪽(`runner.rs`)이 이 호출 전체를 감싸 지킨다.

use crate::model::ModelError;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::time::Duration;

/// 연결을 맺는 시간
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 죽은 상대를 알아채는 TCP keepalive 간격(응답을 기다리는 동안 상대가 사라진 연결)
const KEEPALIVE: Duration = Duration::from_secs(30);
/// 응답 본문의 읽기 상한(넘으면 받지 않는다)
pub(crate) const MAX_BODY: usize = 8 << 20;
/// 오류 응답 본문을 읽는 상한(넘는 부분은 읽지 않는다)
pub(crate) const ERROR_BODY_MAX: usize = 16 << 10;

/// 요청 머리 하나: 이름(소문자)·값·값이 비밀인가
pub(crate) type Header = (&'static str, String, bool);

/// 전송 실패의 분류(값 없이 종류만)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Fault {
    Connect,
    Timeout,
    Body,
    TooLarge,
    Request,
}

impl Fault {
    /// 모델 실패로 옮긴다: 연결·시간 초과·본문 읽기 실패는 일시(`network`), 응답이 한도를 넘었거나 요청을 만들 수 없으면 영구다.
    pub(crate) fn into_error(self) -> ModelError {
        match self {
            Fault::Connect | Fault::Timeout | Fault::Body => ModelError::Failed {
                code: "network",
                transient: true,
            },
            Fault::TooLarge => ModelError::Failed {
                code: "invalid_response",
                transient: false,
            },
            Fault::Request => ModelError::Failed {
                code: "prepare_failed",
                transient: false,
            },
        }
    }
}

/// 받은 응답: 상태와 본문(2xx는 전부, 그 밖의 상태는 상한 `ERROR_BODY_MAX` 안의 앞부분)
pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 상류 HTTP 클라이언트. 복제는 같은 클라이언트를 쓴다.
#[derive(Clone, Debug)]
pub struct ApiClient {
    client: Client,
}

impl ApiClient {
    /// 만든다. `loopback`이면 환경의 프록시 설정을 쓰지 않는다(로컬 서버로 가는 요청이 프록시를 거치지 않는다). 시스템 CA를 읽지 못하면 `Err`이다.
    pub fn new(loopback: bool) -> Result<Self, String> {
        let mut builder = Client::builder()
            // 일부 게이트웨이(WAF)는 User-Agent가 없는 요청을 거절한다
            .user_agent("bot-framework")
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(KEEPALIVE)
            // 닫힌 유휴 연결로 보냈다가 클라이언트가 몰래 다시 보내는 일을 막는다
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(Policy::none());
        if loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| {
            "상류 HTTPS 클라이언트를 만들지 못했어요(CA 인증서가 있는지 확인)".to_owned()
        })?;
        Ok(Self { client })
    }

    /// `url`로 본문을 POST한다. `idle`은 본문 조각 하나를 기다리는 한도다. 2xx가 아닌 응답은 분류용으로 `ERROR_BODY_MAX` 앞부분만 읽는다.
    pub(crate) async fn post(
        &self,
        url: &str,
        headers: Vec<Header>,
        body: Vec<u8>,
        idle: Duration,
    ) -> Result<Answer, Fault> {
        let mut map = HeaderMap::new();
        for (name, value, secret) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Fault::Request)?;
            let mut value = HeaderValue::from_str(&value).map_err(|_| Fault::Request)?;
            value.set_sensitive(secret);
            map.append(name, value);
        }
        let mut response = self
            .client
            .post(url)
            .headers(map)
            .body(body)
            .send()
            .await
            .map_err(|e| fault(&e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_prefix(&mut response, idle, ERROR_BODY_MAX).await;
            return Ok(Answer { status, body });
        }
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(idle, response.chunk()).await {
                Err(_) => return Err(Fault::Timeout),
                Ok(Err(e)) => return Err(fault(&e)),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => {
                    if out.len() + chunk.len() > MAX_BODY {
                        return Err(Fault::TooLarge);
                    }
                    out.extend_from_slice(&chunk);
                }
            }
        }
        Ok(Answer { status, body: out })
    }
}

/// 본문의 앞 `max`바이트까지만 읽는다. 넘는 부분·읽기 실패·시간 초과는 읽은 데까지로 끝낸다(판단에만 쓰는 값이라 실패해도 일반 분류로 떨어진다).
async fn read_prefix(response: &mut reqwest::Response, idle: Duration, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < max {
        match tokio::time::timeout(idle, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let room = max - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            _ => break,
        }
    }
    out
}

/// 실패를 값 없이 분류만 남긴다.
fn fault(error: &reqwest::Error) -> Fault {
    let fault = if error.is_builder() {
        Fault::Request
    } else if error.is_timeout() {
        Fault::Timeout
    } else if error.is_body() || error.is_decode() {
        Fault::Body
    } else {
        // 연결·TLS 핸드셰이크·요청을 쓰다 끊김 등: 응답을 받지 못했다
        Fault::Connect
    };
    tracing::warn!(kind = ?fault, "모델 상류 요청이 실패했어요");
    fault
}
