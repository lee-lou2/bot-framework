//! Slack Web API 호출. 쓰기 호출은 결과를 넷으로 나눠 돌려준다 — 실행 확정(`Ok`), 거절 확정(`Error`), 실행 전 실패
//! (`BeforeWrite`·`RateLimited`·`TokenRejected`), 불명(`Unknown`). 숨은 재전송은 없다: 연결을 재사용하지 않고(끊긴 연결로 보냈다가 몰래
//! 다시 보내는 일이 없게) 429도 기다렸다 다시 보내지 않고 호출한 쪽에 알린다.
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const DEFAULT_BASE: &str = "https://slack.com/api/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE: usize = 1 << 20;
const DEFAULT_RETRY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Raw {
    Ok(Value),
    /// Slack가 이유를 밝히며 거절했다(`ok:false`). 요청은 실행되지 않았다. 코드는 `[a-z0-9_]`만 남긴다.
    Error(String),
    /// 이번 요청의 바이트를 쓰기 전에 연결이 실패했다.
    BeforeWrite,
    /// Slack가 속도 제한으로 거절했다(HTTP 429, 실행 전). 기다릴 시간이 하한이다.
    RateLimited(Duration),
    /// Slack가 봇 토큰 자체를 거절했다(`invalid_auth`·`token_revoked`·`token_expired`). 인증은 요청을 실행하기 전에 걸러지므로 아무것도 실행되지 않았다.
    /// 토큰을 바꾸는 동안에는 보관소의 새 값이 아직 반영되지 않은 옛 토큰이 이 거절을 받으니 영구 거절이 아니다 — 호출한 쪽은 미실행으로 다루고,
    /// 기억한 토큰은 `Credentials::call`이 버려 다음 시도가 자격을 다시 읽게 한다.
    TokenRejected,
    /// 시간 초과·5xx·읽을 수 없는 응답 — 실행됐는지 알 수 없다.
    Unknown,
}

pub enum Body<'a> {
    Json(&'a Value),
    Form(Vec<(&'static str, String)>),
}

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    base: String,
}

fn sanitize(code: &str) -> String {
    let clean = !code.is_empty()
        && code.len() <= 40
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if clean {
        code.to_owned()
    } else {
        "unknown_error".into()
    }
}

/// 응답 한 건을 읽는다(순수). 2xx의 `ok`만 Slack의 답으로 인정한다 — 중간 프록시의 5xx는 본문이 그럴듯해도 불명이다.
pub fn classify(status: u16, retry_after: Option<&str>, body: &[u8]) -> Raw {
    let wait = || {
        retry_after
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_RETRY, Duration::from_secs)
            .max(Duration::from_secs(1))
    };
    if status == 429 {
        return Raw::RateLimited(wait());
    }
    if !(200..300).contains(&status) || body.len() > MAX_RESPONSE {
        return Raw::Unknown;
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Raw::Unknown;
    };
    match value["ok"].as_bool() {
        Some(true) => Raw::Ok(value),
        Some(false) => match value["error"].as_str().filter(|code| !code.is_empty()) {
            Some("ratelimited") => Raw::RateLimited(wait()),
            Some("invalid_auth" | "token_revoked" | "token_expired") => Raw::TokenRejected,
            Some(code) => Raw::Error(sanitize(code)),
            None => Raw::Unknown,
        },
        None => Raw::Unknown,
    }
}

impl SlackApi {
    /// `base`는 시험용 가짜 서버 주소. 생략하면 `https://slack.com/api/`.
    pub fn new(base: Option<&str>) -> Self {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // 연결 재사용을 끈다: 닫힌 유휴 연결로 보냈다가 클라이언트가 몰래 다시 보내는 일을 막는다
            .pool_max_idle_per_host(0)
            // 클라이언트 기본 재시도(프로토콜 수준 NACK 재전송)도 끈다 — 다시 보낼지는 정산 결과(NotSent)가 정한다
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP 클라이언트 초기화 실패(CA 인증서가 있는지 확인)");
        let mut base = base.unwrap_or(DEFAULT_BASE).to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        Self { http, base }
    }

    pub async fn call(&self, method: &str, token: &str, body: Body<'_>) -> Raw {
        let request = self
            .http
            .post(format!("{}{method}", self.base))
            .bearer_auth(token);
        let request = match body {
            Body::Json(value) => request.json(value),
            Body::Form(form) => request.form(&form),
        };
        self.finish(request.send().await).await
    }

    /// 파일 바이트를 올릴 주소로 한 번 보낸다. 주소 자체가 열쇠라 토큰을 붙이지 않고 오류에도 싣지 않는다.
    pub async fn put_bytes(&self, url: &str, bytes: Vec<u8>) -> Raw {
        let sent = self
            .http
            .post(url)
            .timeout(UPLOAD_TIMEOUT)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await;
        match sent {
            Ok(response) if response.status().is_success() => Raw::Ok(Value::Null),
            Ok(response) if response.status().as_u16() == 429 => Raw::RateLimited(DEFAULT_RETRY),
            Ok(_) => Raw::Unknown,
            Err(error) if error.is_connect() => Raw::BeforeWrite,
            Err(_) => Raw::Unknown,
        }
    }

    async fn finish(&self, sent: Result<reqwest::Response, reqwest::Error>) -> Raw {
        let response = match sent {
            Ok(response) => response,
            Err(error) if error.is_connect() => return Raw::BeforeWrite,
            Err(_) => return Raw::Unknown,
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        match response.bytes().await {
            Ok(body) => classify(status, retry_after.as_deref(), &body),
            Err(_) if status == 429 => classify(status, retry_after.as_deref(), b""),
            Err(_) => Raw::Unknown,
        }
    }

    /// Socket Mode 웹소켓 주소(앱 토큰)
    pub async fn open_socket(&self, app_token: &str) -> Option<String> {
        match self
            .call("apps.connections.open", app_token, Body::Form(vec![]))
            .await
        {
            Raw::Ok(value) => value["url"].as_str().map(str::to_owned),
            _ => None,
        }
    }

    /// 봇 토큰이 속한 사용자·팀·앱. 소켓의 앱과 맞는지 확인하는 데 쓴다.
    pub async fn auth_test(&self, bot_token: &str) -> Option<AuthInfo> {
        match self.call("auth.test", bot_token, Body::Form(vec![])).await {
            Raw::Ok(value) => Some(AuthInfo {
                user_id: value["user_id"].as_str()?.to_owned(),
                team_id: value["team_id"].as_str()?.to_owned(),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthInfo {
    pub user_id: String,
    pub team_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(status: u16, retry: Option<&str>, body: &str) -> Raw {
        classify(status, retry, body.as_bytes())
    }

    #[test]
    fn only_a_2xx_ok_json_is_confirmed() {
        assert!(matches!(
            raw(200, None, r#"{"ok":true,"ts":"1.1"}"#),
            Raw::Ok(_)
        ));
        assert_eq!(
            raw(200, None, r#"{"ok":false,"error":"channel_not_found"}"#),
            Raw::Error("channel_not_found".into())
        );
        // 5xx는 본문이 그럴듯해도 불명이다(중간 프록시가 덮었을 수 있다)
        assert_eq!(raw(502, None, r#"{"ok":true}"#), Raw::Unknown);
        assert_eq!(raw(500, None, ""), Raw::Unknown);
        assert_eq!(raw(302, None, ""), Raw::Unknown);
        // 읽을 수 없는 본문·ok 없음·이유 없는 거절
        for body in [
            "",
            "not json",
            r#"{"ts":"1.1"}"#,
            r#"{"ok":false}"#,
            r#"{"ok":"yes"}"#,
        ] {
            assert_eq!(raw(200, None, body), Raw::Unknown, "{body}");
        }
        assert_eq!(
            classify(200, None, &vec![b' '; MAX_RESPONSE + 1]),
            Raw::Unknown
        );
    }

    #[test]
    fn rate_limits_carry_the_wait_and_never_less_than_a_second() {
        assert_eq!(
            raw(429, Some("7"), ""),
            Raw::RateLimited(Duration::from_secs(7))
        );
        assert_eq!(
            raw(429, Some("0"), ""),
            Raw::RateLimited(Duration::from_secs(1))
        );
        assert_eq!(raw(429, None, ""), Raw::RateLimited(DEFAULT_RETRY));
        assert_eq!(raw(429, Some("soon"), ""), Raw::RateLimited(DEFAULT_RETRY));
        assert_eq!(
            raw(200, Some("3"), r#"{"ok":false,"error":"ratelimited"}"#),
            Raw::RateLimited(Duration::from_secs(3))
        );
    }

    #[test]
    fn a_rejected_bot_token_is_not_an_ordinary_refusal() {
        // 토큰 자체를 거절한 코드는 미실행의 일시 실패다(교체 중인 옛 토큰이 받는다). 다른 인증 계열·권한 거절은 보통 거절로 남는다
        for code in ["invalid_auth", "token_revoked", "token_expired"] {
            let body = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert_eq!(raw(200, None, &body), Raw::TokenRejected, "{code}");
        }
        for code in [
            "not_authed",
            "account_inactive",
            "missing_scope",
            "channel_not_found",
            "invalid_blocks",
        ] {
            let body = format!(r#"{{"ok":false,"error":"{code}"}}"#);
            assert_eq!(raw(200, None, &body), Raw::Error(code.into()), "{code}");
        }
        // 2xx의 `ok:false`만 Slack의 답이다: 중간 프록시가 낸 4xx·5xx는 본문이 그럴듯해도 불명이다
        for status in [401, 403, 500, 502] {
            assert_eq!(
                raw(status, None, r#"{"ok":false,"error":"invalid_auth"}"#),
                Raw::Unknown,
                "{status}"
            );
        }
    }

    #[test]
    fn error_codes_are_reduced_to_a_safe_vocabulary() {
        for odd in ["Has Space", "UPPER", &"x".repeat(41), "a-b"] {
            let body = format!(r#"{{"ok":false,"error":"{odd}"}}"#);
            assert_eq!(
                raw(200, None, &body),
                Raw::Error("unknown_error".into()),
                "{odd}"
            );
        }
    }
}
