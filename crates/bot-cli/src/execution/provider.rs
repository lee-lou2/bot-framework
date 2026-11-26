//! 호스트 설정의 `providers` 절: 모델 별칭 → 실행 방법(`Provider`)과 그 모양 검사(docs/architecture.md «호스트 설정과 역할 조립»).
//!
//! - `demo`(결정적 Step, 외부 키·VM 불필요), `claude-cli`(상류 `model`·HTTPS `endpoint`·프로필
//!   자격 이름 `secret`), 또는 API 직접 호출 `anthropic-api`·`openai-api`(VM·CLI 없이 워커가 상류 API를 직접 부른다: 상류 `model`·기준 `endpoint`·프로필 자격 이름 `secret`과 선택 출력 한도).
//!   출력 한도는 `max_tokens`이고, `openai-api`는 `max_completion_tokens`도 쓸 수 있다(둘 중 하나만, 적힌 이름 그대로 상류에 보낸다 — OpenAI 공식 API의 새 모델은 `max_completion_tokens`만 받고
//!   Ollama·vLLM·Gemini 호환 같은 호환 서버는 대개 `max_tokens`를 받는다).
//!   API 직접 호출의 `endpoint`는 `https`만이고 `http`는 loopback IP 리터럴만이며, `secret`은 loopback일 때만 생략한다(`bot_runtime::model::ApiEndpoint`).
//!   `pi`는 격리·재개 어댑터가 준비되기 전에는 쓸 수 없다. URL에 자격·쿼리·조각을 넣을 수 없다.
use super::{object, only, text};
use bot_core::id::SecretName;
use bot_runtime::model::{ApiEndpoint, LimitField};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// 모델 별칭의 실행 방법
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    /// 외부 호출 없는 결정적 데모 모델
    Demo,
    /// 상류 서비스로 가는 Claude CLI. 서비스 키는 브로커가 상류에만 보낸다(`secret`은 프로필 자격 **이름**).
    ClaudeCli {
        model: String,
        endpoint: String,
        secret: String,
    },
    /// 워커가 상류 Anthropic Messages API(`POST {endpoint}/v1/messages`)를 직접 부른다(`RunnerKind::Step`, VM 없음). 모델은 호스트 파일·셸에 닿지 못하고 도구는 워커→브로커 경로만 쓴다.
    /// `secret`은 서비스 키의 프로필 자격 이름이고 loopback 주소에서만 생략할 수 있다. `max_tokens`가 없으면 4096이다.
    AnthropicApi {
        model: String,
        endpoint: String,
        secret: Option<String>,
        max_tokens: Option<u32>,
    },
    /// 워커가 OpenAI 호환 Chat Completions(`POST {endpoint}/chat/completions`, `endpoint`는 `/v1`까지 포함한 기준 주소)를 직접 부른다. OpenAI·Gemini 호환·Ollama·vLLM이 쓴다.
    /// 출력 한도(`max_tokens`)는 `limit_field`가 가리키는 이름(`max_tokens` 또는 `max_completion_tokens`) 그대로 상류에 간다.
    OpenAiApi {
        model: String,
        endpoint: String,
        secret: Option<String>,
        max_tokens: Option<u32>,
        limit_field: LimitField,
    },
}

/// 상류 주소: `https://호스트[:포트][/경로]`. 자격(`user@`)·쿼리·조각은 금지(자격은 SecretStore로만 준다). 호스트와 포트는 실제로
/// 보내는 쪽(브로커의 egress)과 같은 규칙(`HostPort::parse`)으로 해석돼야 한다 — 포트가 범위 밖(70000)·0이거나 호스트를 정규화할
/// 수 없으면 기동 전에 거절한다. 포트를 생략하면 443이다.
fn endpoint_ok(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    if !endpoint.is_ascii() || endpoint.contains(['?', '#', '@', ' ', '\\']) {
        return false;
    }
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    let target = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:443")
    };
    bot_broker::resolve::HostPort::parse(&target).is_some()
}

/// API 직접 호출 항목의 공통 필드
struct ApiFields {
    model: String,
    endpoint: String,
    secret: Option<String>,
    /// 출력 한도 값
    max_tokens: Option<u32>,
    /// 그 값을 실어 보낼 필드 이름(적힌 이름 그대로)
    limit_field: LimitField,
}

/// API 직접 호출 항목(`anthropic-api`·`openai-api`)의 공통 필드. 주소는 `ApiEndpoint` 규칙(https만, http는 loopback IP 리터럴만, 자격·쿼리·조각 금지)을 지키고 키(`secret`)는 loopback
/// 주소에서만 생략할 수 있다. 출력 한도는 `max_tokens`이고, `completion_limit`(OpenAI 호환)이면 `max_completion_tokens`도 받되 둘 중 하나만 쓸 수 있다.
fn parse_api(
    fields: &Map<String, Value>,
    path: &str,
    completion_limit: bool,
) -> Result<ApiFields, String> {
    let allowed: &[&str] = if completion_limit {
        &[
            "kind",
            "model",
            "endpoint",
            "secret",
            "max_tokens",
            "max_completion_tokens",
        ]
    } else {
        &["kind", "model", "endpoint", "secret", "max_tokens"]
    };
    only(fields, allowed, path)?;
    let model = text(fields, "model", path)?;
    let endpoint_text = text(fields, "endpoint", path)?;
    let endpoint =
        ApiEndpoint::parse(endpoint_text).map_err(|reason| format!("{path}.endpoint: {reason}"))?;
    let secret = match fields.get("secret") {
        None => None,
        Some(_) => {
            let name = text(fields, "secret", path)?;
            if SecretName::parse(name).is_none() {
                return Err(format!("{path}.secret: 자격 이름 규칙에 맞지 않아요"));
            }
            Some(name.to_owned())
        }
    };
    if secret.is_none() && !endpoint.is_loopback() {
        return Err(format!(
            "{path}.secret: 서비스 키의 자격 이름이 필요해요(키 없이 쓸 수 있는 것은 loopback IP 주소뿐이에요)"
        ));
    }
    if fields.contains_key("max_tokens") && fields.contains_key("max_completion_tokens") {
        return Err(format!(
            "{path}: max_tokens와 max_completion_tokens는 하나만 쓸 수 있어요"
        ));
    }
    let (name, limit_field) = if fields.contains_key("max_completion_tokens") {
        ("max_completion_tokens", LimitField::MaxCompletionTokens)
    } else {
        ("max_tokens", LimitField::MaxTokens)
    };
    let max_tokens = match fields.get(name) {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|tokens| (1..=1_000_000).contains(tokens))
                .and_then(|tokens| u32::try_from(tokens).ok())
                .ok_or_else(|| format!("{path}.{name}: 1~1000000의 정수여야 해요"))?,
        ),
    };
    Ok(ApiFields {
        model: model.to_owned(),
        endpoint: endpoint_text.to_owned(),
        secret,
        max_tokens,
        limit_field,
    })
}

/// `providers` 절. 별칭은 이름 규칙을 따르고 모르는 키·`pi`는 거절한다.
pub fn parse_providers(value: &Value) -> Result<BTreeMap<String, Provider>, String> {
    let mut out = BTreeMap::new();
    for (alias, entry) in object(value, "providers")? {
        let path = format!("providers.{alias}");
        if !bot_core::id::is_name(alias) {
            return Err(format!(
                "{path}: 별칭이 이름 규칙(소문자·숫자·하이픈)에 맞지 않아요"
            ));
        }
        let fields = object(entry, &path)?;
        let provider = match text(fields, "kind", &path)? {
            "demo" => {
                only(fields, &["kind"], &path)?;
                Provider::Demo
            }
            "claude-cli" => {
                only(fields, &["kind", "model", "endpoint", "secret"], &path)?;
                let endpoint = text(fields, "endpoint", &path)?;
                if !endpoint_ok(endpoint) {
                    return Err(format!(
                        "{path}.endpoint: `https://호스트[:포트][/경로]`여야 해요(자격·쿼리·조각 금지)"
                    ));
                }
                let secret = text(fields, "secret", &path)?;
                if SecretName::parse(secret).is_none() {
                    return Err(format!("{path}.secret: 자격 이름 규칙에 맞지 않아요"));
                }
                Provider::ClaudeCli {
                    model: text(fields, "model", &path)?.to_owned(),
                    endpoint: endpoint.to_owned(),
                    secret: secret.to_owned(),
                }
            }
            "anthropic-api" => {
                let api = parse_api(fields, &path, false)?;
                Provider::AnthropicApi {
                    model: api.model,
                    endpoint: api.endpoint,
                    secret: api.secret,
                    max_tokens: api.max_tokens,
                }
            }
            "openai-api" => {
                let api = parse_api(fields, &path, true)?;
                Provider::OpenAiApi {
                    model: api.model,
                    endpoint: api.endpoint,
                    secret: api.secret,
                    max_tokens: api.max_tokens,
                    limit_field: api.limit_field,
                }
            }
            "pi" => {
                return Err(format!(
                    "{path}.kind: pi는 격리·재개 어댑터가 준비되기 전에는 쓸 수 없어요"
                ));
            }
            other => {
                return Err(format!(
                    "{path}.kind: {other}는 지원하지 않아요(demo · claude-cli · anthropic-api · openai-api)"
                ));
            }
        };
        out.insert(alias.clone(), provider);
    }
    Ok(out)
}
