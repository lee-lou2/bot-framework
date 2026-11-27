//! 호스트 설정의 `providers` 절: 모델 별칭 → 실행 방법(`Provider`)과 그 모양 검사(docs/architecture.md «호스트 설정과 역할 조립»).
//!
//! - `demo`(결정적 Step, 외부 키·VM 불필요), `claude-cli`(상류 `model`·HTTPS `endpoint`·프로필
//!   자격 이름 `secret`), 또는 API 직접 호출 `anthropic-api`·`openai-api`(VM·CLI 없이 워커가 상류 API를 직접 부른다: 상류 `model`·기준 `endpoint`·프로필 자격 이름 `secret`과 선택 출력 한도).
//!   출력 한도는 `max_tokens`이고, `openai-api`는 `max_completion_tokens`도 쓸 수 있다(둘 중 하나만, 적힌 이름 그대로 상류에 보낸다 — OpenAI 공식 API의 새 모델은 `max_completion_tokens`만 받고
//!   Ollama·vLLM·Gemini 호환 같은 호환 서버는 대개 `max_tokens`를 받는다).
//!   API 직접 호출의 `endpoint`는 `https`만이고 `http`는 loopback IP 리터럴만이며, `secret`은 loopback일 때만 생략한다(`bot_runtime::model::ApiEndpoint`).
//!   `pi`는 격리·재개 어댑터가 준비되기 전에는 쓸 수 없다. URL에 자격·쿼리·조각을 넣을 수 없다.
//!
//! 종류마다 달라지는 **사실**(서비스 키 자격 이름·활성화 지문 재료·운영자용 설명·모델 VM 필요 여부)은 `impl Provider` 한 블록에 있다. `validate`·`doctor`·worker 시작·활성화
//! 지문은 그 메서드를 부르므로, 새 종류를 더하면 컴파일러가 이 블록의 `match`를 모두 가리킨다(실행기를 만드는 `serve::models::build`는 실행 방식 세 가지로만 가른다).
use super::{field, object, only, text};
use bot_core::id::SecretName;
use bot_runtime::model::{ApiEndpoint, ApiKind, Effort, LimitField};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
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
        /// 사고(추론) 수준(`--effort`). 생략하면 기본 `medium`이다.
        effort: Option<Effort>,
    },
    /// 워커가 상류 API를 직접 부른다(`RunnerKind::Step`, VM 없음). 모델은 호스트 파일·셸에 닿지 못하고 도구는 워커→브로커 경로만 쓴다.
    Api(ApiProvider),
}

/// API 직접 호출 별칭(`anthropic-api`·`openai-api`)의 설정. 실행기가 받는 `bot_runtime::model::ApiSettings`와 같은 모양이되, 주소와 자격 이름을 적힌 글 그대로 들고 있다
/// (활성화 지문이 기준 주소 전체를 적힌 그대로 해시한다).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiProvider {
    /// 상류 규약: `Anthropic`은 `POST {endpoint}/v1/messages`, `OpenAi`는 OpenAI 호환 Chat Completions(`POST {endpoint}/chat/completions`, `endpoint`는 `/v1`까지 포함한 기준 주소)다.
    pub kind: ApiKind,
    pub model: String,
    pub endpoint: String,
    /// 서비스 키의 프로필 자격 이름. loopback 주소에서만 생략할 수 있다.
    pub secret: Option<String>,
    /// 출력 한도. 없으면 Anthropic은 4096이고 OpenAI 호환은 보내지 않는다.
    pub max_tokens: Option<u32>,
    /// `max_tokens` 값을 실어 보낼 필드 이름(적힌 이름 그대로). OpenAI 호환만 `max_completion_tokens`를 고를 수 있다.
    pub limit_field: LimitField,
    /// 사고(추론) 수준. 생략하면 상류 요청에 넣지 않는다.
    pub effort: Option<Effort>,
}

impl Provider {
    /// 서비스 키의 프로필 자격 이름(키가 필요 없으면 None). 이 이름을 보관소에서 못 찾으면 모든 모델 요청이 런타임에 실패하므로 `bot validate`와 worker 시작이 같이 점검한다.
    pub fn secret(&self) -> Option<&str> {
        match self {
            Provider::Demo => None,
            Provider::ClaudeCli { secret, .. } => Some(secret),
            Provider::Api(api) => api.secret.as_deref(),
        }
    }

    /// 모델 CLI를 도는 VM(`sandbox.apple`)이 필요한가. 필요한데 호스트에 VM이 없으면 worker가 시작하지 않는다(다른 샌드박스로 대신하지 않는다).
    pub fn needs_model_vm(&self) -> bool {
        match self {
            Provider::Demo | Provider::Api(_) => false,
            Provider::ClaudeCli { .. } => true,
        }
    }

    /// `bot doctor`가 보여 주는 한 줄: 자격 **이름**(값 아님)과 상류 호스트만 싣는다. 주소의 경로·쿼리는 싣지 않는다. API 직접 호출은 출력 한도를 적었을 때만 보이고,
    /// 기본 필드(`max_tokens`)가 아니면 상류에 보내는 이름도 함께 보인다.
    pub fn describe(&self, alias: &str) -> String {
        match self {
            Provider::Demo => format!("{alias}: demo(외부 호출 없는 데모 모델)"),
            Provider::ClaudeCli {
                model,
                endpoint,
                secret,
                effort,
            } => {
                let effort_note = effort.map_or_else(String::new, |e| format!(" · 사고 수준 {e}"));
                format!(
                    "{alias}: claude-cli · 모델 {model} · 상류 {} · 자격 이름 {secret}{effort_note}",
                    upstream_host(endpoint)
                )
            }
            Provider::Api(api) => {
                let kind = match api.kind {
                    ApiKind::Anthropic => "anthropic-api",
                    ApiKind::OpenAi => "openai-api",
                };
                let key = api.secret.as_deref().map_or_else(
                    || "서비스 키 없음(loopback 전용)".to_owned(),
                    |name| format!("자격 이름 {name}"),
                );
                let limit =
                    api.max_tokens
                        .map_or_else(String::new, |tokens| match api.limit_field {
                            LimitField::MaxTokens => format!(" · 최대 토큰 {tokens}"),
                            other => format!(" · 최대 토큰 {tokens}({})", other.name()),
                        });
                let effort_note = api
                    .effort
                    .map_or_else(String::new, |e| format!(" · 사고 수준 {e}"));
                format!(
                    "{alias}: {kind} · 모델 {} · 상류 {} · {key}{limit}{effort_note}",
                    api.model,
                    upstream_host(&api.endpoint)
                )
            }
        }
    }

    /// 활성화 지문에 넣는 권한 재료(`authority_fingerprint`가 별칭마다 부른다): 종류 표지·상류 모델·기준 주소 전체·자격 이름(없으면 표지만). 자격 값과 호스트별 경로는 넣지 않는다.
    /// 바이트가 달라지면 이미 활성화된 모든 배포가 재활성화를 요구받는다(`execution/fingerprint_tests.rs`가 고정한다).
    pub(super) fn authority(&self, hash: &mut Sha256) {
        match self {
            Provider::Demo => hash.update([0]),
            Provider::ClaudeCli {
                model,
                endpoint,
                secret,
                effort: _,
            } => {
                hash.update([1]);
                for part in [model, endpoint, secret] {
                    field(hash, part.as_bytes());
                }
            }
            // 출력 한도(`max_tokens`·`limit_field`)와 사고 수준(`effort`)은 권한이 아니라 운영 값이다. 필드를 모두 이름으로 적어 둬서 새 필드가 지문에서 조용히 빠지지 않는다(더하면 컴파일 오류).
            Provider::Api(ApiProvider {
                kind,
                model,
                endpoint,
                secret,
                max_tokens: _,
                limit_field: _,
                effort: _,
            }) => {
                hash.update([match kind {
                    ApiKind::Anthropic => 2,
                    ApiKind::OpenAi => 3,
                }]);
                field(hash, model.as_bytes());
                field(hash, endpoint.as_bytes());
                match secret {
                    Some(name) => {
                        hash.update([1]);
                        field(hash, name.as_bytes());
                    }
                    None => hash.update([0]),
                }
            }
        }
    }
}

/// 상류 주소에서 호스트(권한부)만 보여 준다. 경로·쿼리는 출력하지 않는다.
fn upstream_host(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// 상류 주소: `https://호스트[:포트][/경로]`. 자격(`user@`)·쿼리·조각·퍼센트 인코딩·공백·제어 문자·IP 변형 표기(`0x`·숫자뿐인 마지막 레이블)는 금지.
/// 호스트와 포트는 실제로 보내는 쪽(브로커의 egress)과 같은 규칙(`HostPort::parse`)으로 해석돼야 한다 — 포트가 범위 밖(70000)·0이거나 호스트를 정규화할
/// 수 없으면 기동 전에 거절한다. 포트를 생략하면 443이다.
fn endpoint_ok(endpoint: &str) -> bool {
    let Some(rest) = endpoint.strip_prefix("https://") else {
        return false;
    };
    if !endpoint.is_ascii()
        || endpoint
            .bytes()
            .any(|b| b <= b' ' || b == 0x7f || b == b'\\')
        || endpoint.contains(['?', '#', '@', '%'])
    {
        return false;
    }
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    let target = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:443")
    };
    let Some(parsed) = bot_broker::resolve::HostPort::parse(&target) else {
        return false;
    };
    if parsed.host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    let Some(last) = parsed.host.rsplit('.').next() else {
        return false;
    };
    !(last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit())))
}

fn parse_effort(fields: &Map<String, Value>, path: &str) -> Result<Option<Effort>, String> {
    let Some(value) = fields.get("effort") else {
        return Ok(None);
    };
    let raw = value.as_str().ok_or_else(|| {
        format!("{path}.effort: low · medium · high · xhigh · max 중 하나여야 해요")
    })?;
    Effort::parse(raw)
        .map(Some)
        .ok_or_else(|| format!("{path}.effort: low · medium · high · xhigh · max 중 하나여야 해요"))
}

/// API 직접 호출 항목(`anthropic-api`·`openai-api`)의 공통 필드. 주소는 `ApiEndpoint` 규칙(https만, http는 loopback IP 리터럴만, 자격·쿼리·조각 금지)을 지키고 키(`secret`)는 loopback
/// 주소에서만 생략할 수 있다. 출력 한도는 `max_tokens`이고, OpenAI 호환은 `max_completion_tokens`도 받되 둘 중 하나만 쓸 수 있다(종류마다 받는 키는 아래 `match`가 정한다).
fn parse_api(
    fields: &Map<String, Value>,
    path: &str,
    kind: ApiKind,
) -> Result<ApiProvider, String> {
    let allowed: &[&str] = match kind {
        ApiKind::Anthropic => &[
            "kind",
            "model",
            "endpoint",
            "secret",
            "max_tokens",
            "effort",
        ],
        ApiKind::OpenAi => &[
            "kind",
            "model",
            "endpoint",
            "secret",
            "max_tokens",
            "max_completion_tokens",
            "effort",
        ],
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
    let effort = parse_effort(fields, path)?;
    Ok(ApiProvider {
        kind,
        model: model.to_owned(),
        endpoint: endpoint_text.to_owned(),
        secret,
        max_tokens,
        limit_field,
        effort,
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
                only(
                    fields,
                    &["kind", "model", "endpoint", "secret", "effort"],
                    &path,
                )?;
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
                let effort = parse_effort(fields, &path)?;
                Provider::ClaudeCli {
                    model: text(fields, "model", &path)?.to_owned(),
                    endpoint: endpoint.to_owned(),
                    secret: secret.to_owned(),
                    effort,
                }
            }
            "anthropic-api" => Provider::Api(parse_api(fields, &path, ApiKind::Anthropic)?),
            "openai-api" => Provider::Api(parse_api(fields, &path, ApiKind::OpenAi)?),
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
