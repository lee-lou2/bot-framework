//! API 직접 호출 실행기 본체. 한 차례(`step`)는 `요청 만들기 → (판본 확인 → 키 읽기 → POST) → 응답 분류·읽기`다.
//!
//! - 취소 신호가 오면 요청을 끊고 `Cancelled`, 전체 시간(`Limits::total`)을 넘기면 `Timeout`이다. 요청을 보내는 순간부터 응답을 다 읽을 때까지가 이 시간 안이다.
//! - **구판 보호**: `SendGate`로 이 프로세스의 활성화 판본이 저장소의 활성 판본인지 확인한다(`serve/guard/upstream.rs`의 `GuardedUpstream::confirm`과 같은 규칙: 마감 안의 성공만 받는다). 확인은
//!   **두 번**이다 — 키를 읽기 전(구판은 키도 읽지 않는다)과 키를 읽은 뒤 보내기 직전(읽는 사이에 판본이 바뀌었으면 키를 버린다). 기존 경로도 전송 동안 가드를 쥐지 않고 보내기 직전에만 확인한다.
//!   확인하지 못하면 아무것도 보내지 않는다. 실패는 일시 오류(`stale_worker`·`activation_unverified`)라 워커·저장소가 구판이면 시도를 쓰지 않고 대기열로 돌려준다.
//! - **키**: 보내는 순간 프로필의 보관소에서 읽는다(교체한 새 값이 바로 쓰인다). 호스트가 준 보관소는 가림 장치에 읽은 값을 등록한다. 값은 요청 머리에만 싣고, 해당 요청의 키가 응답에
//!   있으면 어떤 글도 체크포인트·브로커에 넘기지 않고 `secret_in_response`로 끝낸다(전체 프로필 공유 가림 목록으로 출력을 고치지 않는다). 검사는 원문과 풀어 읽은 모든 글이다(`scan.rs`).
//! - HTTP 상태 분류: 2xx만 응답으로 읽고, 3xx `upstream_redirect`·401/403 `upstream_auth`·그 밖의 4xx `upstream_rejected`는 영구, 408 `network`·429 `rate_limited`·5xx `overloaded`는 일시다.
//!   429의 본문(상한 안에서만 읽는다)이 `insufficient_quota`면 쿼터 소진이라 영구(`quota_exhausted`)다. 본문은 판단에만 쓰고 어디에도 남기지 않는다.

use super::{
    endpoint::ApiEndpoint,
    scan::Needles,
    transport::ApiClient,
    wire::{self, ApiKind, Built, fail},
};
use crate::model::{
    ModelError, ModelRequest, ModelRunner, ModelStep, RunnerKind, StepFuture, driver::cancelled,
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, Reply, SecretStore},
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

/// 서비스 키로 쓸 수 있는 최소 길이. 이보다 짧은 값은 응답에 우연히 들어 있어도 가려낼 수 없으므로(오탐) 키로 쓰지 않는다.
const MIN_KEY_BYTES: usize = 8;
/// 새 요청 직전의 판본 확인을 기다리는 한도(끝나지 않는 확인이 요청을 붙잡지 못하게)
const GATE_WAIT: Duration = Duration::from_secs(10);

/// 출력 토큰 상한을 상류에 알리는 필드 이름. OpenAI 공식 API의 새 모델(o3 등)은 `max_completion_tokens`만 받고, 호환 서버(Ollama·vLLM·Gemini 호환)는 대개 `max_tokens`를 받는다.
/// 운영자가 적은 이름 그대로 보낸다(Anthropic은 언제나 `max_tokens`다).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LimitField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl LimitField {
    /// 요청 본문의 필드 이름
    pub fn name(self) -> &'static str {
        match self {
            LimitField::MaxTokens => "max_tokens",
            LimitField::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// 모델 별칭 하나의 상류 설정(호스트 설정의 `providers.<별칭>`).
#[derive(Clone, Debug)]
pub struct ApiSettings {
    pub kind: ApiKind,
    pub endpoint: ApiEndpoint,
    /// 상류 모델 ID
    pub model: String,
    /// 서비스 키의 프로필 자격 이름. `None`은 loopback 주소(키 없는 로컬 서버)에서만 허용한다.
    pub secret: Option<SecretName>,
    /// 한 응답의 최대 출력 토큰. Anthropic은 필수 필드라 `None`이면 4096이고, OpenAI 호환은 `None`이면 보내지 않는다.
    pub max_tokens: Option<u32>,
    /// `max_tokens` 값을 실어 보내는 필드 이름(OpenAI 호환만 고를 수 있다)
    pub limit_field: LimitField,
}

impl ApiSettings {
    /// 이 설정의 공급자 지문: 종류·기준 주소 전체·모델·자격 이름. 공급자가 준 불투명 상태에 묶어 두었다가, 같은 공급자일 때만 되돌린다.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{:?}|{}|{}|{}",
            self.kind,
            self.endpoint.url(""),
            self.model,
            self.secret.as_ref().map_or("", |name| name.as_str())
        )
    }
}

/// 요청을 보내기 직전의 확인. 호스트가 «이 프로세스의 활성화 판본이 저장소의 활성 판본인가»에 잇는다.
pub trait SendGate: Send + Sync {
    /// 같으면 `Ok`, 다르거나 아직 게시되지 않았으면 `Stale`, 저장소를 읽지 못하면 `Unavailable`.
    fn verify(&self) -> Reply<'_, ()>;
}

/// API 직접 호출 실행기(`RunnerKind::Step`). 별칭마다 하나를 만든다.
pub struct ApiRunner {
    settings: ApiSettings,
    secrets: Arc<dyn SecretStore>,
    gate: Arc<dyn SendGate>,
    client: ApiClient,
}

impl ApiRunner {
    /// 만든다. 키 없이 쓸 수 있는 것은 loopback 주소뿐이고, 모델 ID는 비어 있을 수 없다. 보관소는 구체 형으로 받는다(호스트가 `Arc<dyn SecretStore>`를 역할 입력으로 돌리지 않게).
    pub fn new<S: SecretStore + 'static>(
        settings: ApiSettings,
        secrets: Arc<S>,
        gate: Arc<dyn SendGate>,
    ) -> Result<Self, String> {
        if settings.secret.is_none() && !settings.endpoint.is_loopback() {
            return Err("서비스 키(secret) 없이 쓸 수 있는 주소는 loopback뿐이에요".to_owned());
        }
        if settings.model.trim().is_empty() {
            return Err("모델 ID가 비어 있어요".to_owned());
        }
        let client = ApiClient::new(settings.endpoint.is_loopback())?;
        Ok(Self {
            settings,
            secrets,
            gate,
            client,
        })
    }

    /// 프로필의 서비스 키를 읽는다. 못 찾거나 쓸 수 없는 값이면 고정 코드로 실패한다(값은 어디에도 남기지 않는다).
    async fn key(&self, profile: &ProfileId, name: &SecretName) -> Result<String, ModelError> {
        let unusable = |error: Error| match error {
            Error::Unavailable => fail("secret_unavailable", true),
            _ => fail("model_key_missing", false),
        };
        let handle = self
            .secrets
            .for_profile(profile.clone())
            .await
            .map_err(unusable)?;
        let value = handle.resolve(name.clone()).await.map_err(unusable)?;
        let text = std::str::from_utf8(&value.0)
            .map_err(|_| fail("model_key_invalid", false))?
            .trim()
            .to_owned();
        // 머리 값이 될 수 있는 보이는 ASCII만(줄바꿈·공백이 든 값으로 머리를 쪼갤 수 없다)
        if text.len() < MIN_KEY_BYTES || !text.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(fail("model_key_invalid", false));
        }
        Ok(text)
    }

    /// 판본 확인(`GuardedUpstream::confirm`과 같은 규칙): 끝나지 않는 확인이 요청을 붙잡지 못하게 `GATE_WAIT`까지만 기다리고, 마감을 넘겨 도착한 성공은 받지 않는다(마감과 완료가 같은 때에
    /// 준비돼도 시각을 마감과 대조한다). 늦게 도착한 거절(`Stale`)은 그대로 받는다.
    async fn confirm(&self) -> Result<(), ModelError> {
        let limit = Instant::now() + GATE_WAIT;
        match timeout_at(limit, self.gate.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            Ok(Err(Error::Stale)) => Err(fail("stale_worker", true)),
            _ => Err(fail("activation_unverified", true)),
        }
    }

    /// 판본 확인 → 키 읽기 → 판본 확인 → POST. 첫 확인이 «구판은 키도 읽지 않는다»를, 둘째 확인이 «키를 읽는 사이에 철회된 목적지는 키도 대화도 받지 않는다»를 지킨다.
    async fn send(&self, request: &ModelRequest, body: Vec<u8>) -> Result<Sent, ModelError> {
        self.confirm().await?;
        let key = match &self.settings.secret {
            Some(name) => {
                let key = self.key(&request.attempt.profile, name).await?;
                // 키를 읽는 동안 활성 판본이 바뀌었을 수 있다: 달라졌으면 읽은 키를 버리고(반환으로 떨어진다) 아무것도 보내지 않는다
                self.confirm().await?;
                Some(key)
            }
            None => None,
        };
        let headers = self.settings.kind.headers(key.as_deref());
        let url = self.settings.endpoint.url(self.settings.kind.path());
        match self
            .client
            .post(&url, headers, body, request.limits.idle)
            .await
        {
            Ok(answer) => Ok(Sent {
                key,
                status: answer.status,
                body: answer.body,
            }),
            Err(fault) => Err(fault.into_error()),
        }
    }

    async fn run(&self, request: ModelRequest) -> Result<ModelStep, ModelError> {
        if *request.cancel.borrow() {
            return Err(ModelError::Cancelled);
        }
        // 이 실행기에는 내장 파일 도구가 없다: 에이전트가 읽기 도구를 요구하면 다른 도구로 바꿔 열지 않고 아무것도 보내기 전에 거절한다(CLI 실행기와 같은 코드)
        if !request.builtin.is_empty() {
            return Err(fail("unsupported_builtin", false));
        }
        let Built { body, names } = self.settings.kind.build(&self.settings, &request)?;
        let mut cancel = request.cancel.clone();
        let deadline = Instant::now() + request.limits.total;
        let sent = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => return Err(ModelError::Cancelled),
            sent = timeout_at(deadline, self.send(&request, body)) => match sent {
                Ok(sent) => sent?,
                Err(_) => return Err(ModelError::Timeout),
            },
        };
        if let Some(error) = status_error(sent.status, &sent.body) {
            if let ModelError::Failed { code, transient } = &error {
                tracing::warn!(
                    code,
                    transient,
                    status = sent.status,
                    error_type = known_error_type(&sent.body),
                    "모델 상류가 오류 상태를 돌려줬어요"
                );
            }
            return Err(error);
        }
        let text = std::str::from_utf8(&sent.body).map_err(|_| wire::invalid())?;
        // 키는 요청 머리에만 있었다. 응답에 있으면(원문이든 이스케이프를 푼 글이든 인자·공급자 상태 안이든) 아무것도 넘기지 않고 닫는다
        let needles = sent.key.as_deref().map(Needles::new);
        if needles
            .as_ref()
            .is_some_and(|needles| needles.in_body(text))
        {
            return Err(fail("secret_in_response", false));
        }
        let step = self
            .settings
            .kind
            .parse(&self.settings, &sent.body, &request, &names)?;
        if needles
            .as_ref()
            .is_some_and(|needles| needles.in_step(&step))
        {
            return Err(fail("secret_in_response", false));
        }
        Ok(step)
    }
}

impl ModelRunner for ApiRunner {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(self.run(request))
    }
}

/// 보낸 요청의 결과: 쓴 키(응답 점검용)와 상태·본문
struct Sent {
    key: Option<String>,
    status: u16,
    body: Vec<u8>,
}

/// HTTP 상태를 실패로 옮긴다. 2xx는 `None`이다. 429는 본문이 쿼터 소진을 말하면 영구다.
fn status_error(status: u16, body: &[u8]) -> Option<ModelError> {
    // 겹치는 범위(`400..=499` 안의 401·408·429)는 match가 아니라 차례로 가른다
    let (code, transient) = if (200..300).contains(&status) {
        return None;
    } else if (300..400).contains(&status) {
        ("upstream_redirect", false)
    } else if status == 401 || status == 403 {
        ("upstream_auth", false)
    } else if status == 408 {
        ("network", true)
    } else if status == 429 {
        if quota_exhausted(body) {
            ("quota_exhausted", false)
        } else {
            ("rate_limited", true)
        }
    } else if (500..600).contains(&status) {
        ("overloaded", true)
    } else if (400..500).contains(&status) {
        ("upstream_rejected", false)
    } else {
        ("invalid_response", false)
    };
    Some(fail(code, transient))
}

/// 429 본문이 쿼터 소진을 말하는가(`error.code`나 `error.type`이 `insufficient_quota`). 일시적인 속도 제한과 달리 기다려도 풀리지 않는다. 본문은 판단에만 쓴다.
fn quota_exhausted(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    ["code", "type"].into_iter().any(|field| {
        value
            .pointer(&format!("/error/{field}"))
            .and_then(Value::as_str)
            == Some("insufficient_quota")
    })
}

/// 상류 오류 본문에서 허용 목록의 고정 분류 낱말만 고른다(원문·메시지는 로그에 싣지 않는다).
fn known_error_type(body: &[u8]) -> &'static str {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return "unknown";
    };
    for pointer in ["/error/type", "/error/code"] {
        if let Some(raw) = value.pointer(pointer).and_then(Value::as_str)
            && let Some(known) = classify_error_type(raw)
        {
            return known;
        }
    }
    "unknown"
}

fn classify_error_type(raw: &str) -> Option<&'static str> {
    match raw {
        "invalid_request_error" => Some("invalid_request_error"),
        "authentication_error" => Some("authentication_error"),
        "permission_error" => Some("permission_error"),
        "not_found_error" => Some("not_found_error"),
        "request_too_large" => Some("request_too_large"),
        "rate_limit_error" => Some("rate_limit_error"),
        "api_error" => Some("api_error"),
        "overloaded_error" => Some("overloaded_error"),
        "insufficient_quota" => Some("insufficient_quota"),
        "billing_error" => Some("billing_error"),
        "server_error" => Some("server_error"),
        "model_not_found" => Some("model_not_found"),
        "context_length_exceeded" => Some("context_length_exceeded"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::known_error_type;

    #[test]
    fn known_error_type_only_returns_allowlisted_classifications() {
        assert_eq!(
            known_error_type(
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"secret-body"}}"#
            ),
            "invalid_request_error"
        );
        assert_eq!(
            known_error_type(br#"{"error":{"code":"insufficient_quota"}}"#),
            "insufficient_quota"
        );
        assert_eq!(
            known_error_type(br#"{"error":{"type":"sk-leaked-secret","code":"arbitrary_text"}}"#),
            "unknown"
        );
        assert_eq!(known_error_type(b"not json"), "unknown");
    }
}
