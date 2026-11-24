//! 모델 제공자 프록시. 모델 샌드박스는 비밀도 네트워크도 없이 브로커가 연 한 통로로만 모델을 쓴다.
//!
//! 브로커가 호스트에서 대신 호출한다. 제공자 키는 이 모듈 안에서만 풀려 상류 요청 머리에만 들어가고
//! 샌드박스로 가는 응답에는 어떤 경로로도 실리지 않는다.
//! - 제공자는 운영자가 프로필마다 허용한 목록에서만 고른다. 목록 밖이면 접속하지 않는다.
//! - 상류 모델 이름은 운영자의 별칭 표로 정한다(샌드박스가 보낸 `model`은 덮어쓴다).
//! - 메시지마다 run·세대·claim을 원장에 대조한다. 옛 소유자의 요청은 상류까지 가지 않는다.
//! - 샌드박스가 보낸 머리는 운영자가 제공자마다 허용한 이름만 모양을 검사해 넘긴다(예: API 판본).
//! - 응답에 키가 섞여 있으면 막고, 상류 오류의 본문은 상태만 남겨 돌려준다.
//!
//! 스트리밍(SSE)은 `forward_stream`이 이벤트 단위로 검사하며 중계한다(`stream` 모듈). 실제 HTTP(TLS) 전송은
//! `Upstream` 포트를 구현하는 쪽(조립하는 곳)이 맡는다.

mod stream;
mod vet;

pub(crate) use stream::error_event;
pub use stream::{ChunkSource, ModelStream, UpstreamStream};
use vet::vet_request;

use crate::{
    canon::canonicalize,
    guard::contains_credential,
    json::parse,
    rate::{Limit, Limiter, RateConfig, Slot},
    resolve::HostPort,
    sys::Clock,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::CallLedger,
    port::{Error, Reply, SecretStore},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

#[derive(Clone, Debug)]
pub struct Provider {
    pub endpoint: HostPort,
    pub path: String,
    /// 프로필 이름공간의 자격 이름. 값은 상류 요청 머리에만 들어간다.
    pub secret: SecretName,
    /// 키를 싣는 머리 이름(`authorization`, `x-api-key` 등)과 값 앞 접두(`Bearer` 등).
    pub header: String,
    pub scheme: Option<String>,
    /// 비밀이 아닌 고정 머리(API 판본 등).
    pub headers: Vec<(String, String)>,
    /// 샌드박스가 보낸 머리 중 그대로 넘길 이름(소문자, 예: `anthropic-version`). 이 밖의 머리는 버린다.
    pub forward_headers: Vec<String>,
    /// 모델 별칭 → 상류 모델 이름.
    pub models: BTreeMap<String, String>,
    /// 기본 허용 목록에 더해 이 제공자가 받는 최상위 요청 필드(운영자가 정한다).
    pub body_fields: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelHost {
    pub providers: BTreeMap<String, Provider>,
    /// 프로필 → 운영자가 허용한 제공자 이름.
    pub allow: BTreeMap<ProfileId, BTreeSet<String>>,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelLimits {
    pub max_request: usize,
    pub max_response: usize,
    pub timeout: Duration,
    pub rate: RateConfig,
    /// 스트림: 조각 사이 최대 대기, 전체 수명, 이벤트 하나·전체 크기.
    pub stream_idle: Duration,
    pub stream_total: Duration,
    pub max_event: usize,
    pub max_stream: usize,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            max_request: 4 << 20,
            max_response: 8 << 20,
            timeout: Duration::from_secs(120),
            rate: RateConfig {
                per_minute: 120,
                burst: 30,
                concurrent: 4,
            },
            stream_idle: Duration::from_secs(60),
            stream_total: Duration::from_secs(900),
            max_event: 1 << 20,
            max_stream: 32 << 20,
        }
    }
}

/// 샌드박스가 보낸 모델 요청. `alias`는 호스트가 세션에 고정한 프로필의 모델 별칭이다.
pub struct ModelRequest {
    pub attempt: Attempt,
    pub alias: String,
    pub body: JsonPayload,
    /// 샌드박스가 보낸 머리. 제공자가 허용한 이름만 넘어간다.
    pub headers: Vec<(String, String)>,
}

pub struct ModelResponse {
    pub status: u16,
    pub body: JsonPayload,
}

pub struct UpstreamRequest {
    pub endpoint: HostPort,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct UpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 상류(모델 제공자)로 보내는 전송 포트. TLS·재시도 금지(숨은 재전송은 쓰지 않는다)는 구현이 지킨다.
pub trait Upstream: Send + Sync {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse>;
    /// 본문을 조각 단위로 흘려 받는다(SSE). 응답 머리를 받으면 돌려준다.
    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream>;
}

pub struct ModelProxy {
    host: ModelHost,
    ledger: Arc<dyn CallLedger>,
    secrets: Arc<dyn SecretStore>,
    upstream: Arc<dyn Upstream>,
    limiter: Arc<Limiter>,
    limits: ModelLimits,
}

/// 상류 호출 직전까지 마친 것. `slot`은 호출이 끝날 때까지 동시성 자리를 쥔다.
struct Prepared {
    slot: Slot,
    request: UpstreamRequest,
    key: Vec<u8>,
}

/// 머리 값은 보이는 ASCII만, 512바이트까지.
fn header_value_ok(value: &str) -> bool {
    value.len() <= 512 && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

impl ModelProxy {
    pub fn new(
        host: ModelHost,
        ledger: Arc<dyn CallLedger>,
        secrets: Arc<dyn SecretStore>,
        upstream: Arc<dyn Upstream>,
        clock: Arc<dyn Clock>,
        limits: ModelLimits,
    ) -> Self {
        Self {
            host,
            ledger,
            secrets,
            upstream,
            limiter: Limiter::new(limits.rate, clock),
            limits,
        }
    }

    /// 프로필이 허용한 제공자 중 이 별칭을 가진 것 하나와 상류 모델 이름. 없거나 둘 이상이면 `None`.
    fn provider(&self, profile: &ProfileId, alias: &str) -> Option<(&Provider, &String)> {
        let allowed = self.host.allow.get(profile)?;
        let mut found = allowed
            .iter()
            .filter_map(|name| self.host.providers.get(name))
            .filter_map(|p| p.models.get(alias).map(|model| (p, model)));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// 이 시도가 지금도 현재 claim을 쥐고 있는지 원장에 대조한다(세션이 도구 목록 같은 읽기에서도 같은 기준을 쓰게 한다).
    pub async fn verify(&self, attempt: Attempt) -> Result<bot_core::state::RunContext, Error> {
        self.ledger.context(attempt).await
    }

    /// 세대·권한 확인, 제공자 선택, 속도 제한, 요청 본문·머리 구성, 키 해석까지.
    async fn prepare(&self, req: ModelRequest, streaming: bool) -> Result<Prepared, Error> {
        let profile = req.attempt.profile.clone();
        let ctx = self.ledger.context(req.attempt).await?;
        let (provider, model) = self.provider(&profile, &req.alias).ok_or(Error::Denied)?;
        let slot = self
            .limiter
            .admit(&profile, &ctx.principal)
            .map_err(|limit| match limit {
                Limit::Rate | Limit::Concurrency => Error::Unavailable,
            })?;
        let body = self.request_body(&req.body, model, streaming, &provider.body_fields)?;
        let key = self
            .secrets
            .for_profile(profile)
            .await?
            .resolve(provider.secret.clone())
            .await
            .map_err(|_| Error::Unavailable)?
            .0;
        let secret = String::from_utf8(key.clone()).map_err(|_| Error::Unavailable)?;
        let mut headers = vec![
            (
                provider.header.clone(),
                match &provider.scheme {
                    Some(scheme) => format!("{scheme} {secret}"),
                    None => secret,
                },
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
        ];
        if streaming {
            headers.push(("accept".to_owned(), "text/event-stream".to_owned()));
        }
        headers.extend(provider.headers.iter().cloned());
        for (name, value) in &req.headers {
            let name = name.to_ascii_lowercase();
            let fixed = provider
                .headers
                .iter()
                .any(|(fixed, _)| fixed.eq_ignore_ascii_case(&name));
            if provider.forward_headers.contains(&name) && !fixed && header_value_ok(value) {
                headers.push((name, value.clone()));
            }
        }
        Ok(Prepared {
            slot,
            request: UpstreamRequest {
                endpoint: provider.endpoint.clone(),
                path: provider.path.clone(),
                headers,
                body: body.into_bytes(),
            },
            key,
        })
    }

    pub async fn forward(&self, req: ModelRequest) -> Result<ModelResponse, Error> {
        let Prepared {
            slot: _slot,
            request,
            key,
        } = self.prepare(req, false).await?;
        let response = timeout(self.limits.timeout, self.upstream.send(request))
            .await
            .map_err(|_| Error::Unavailable)??;
        self.response_body(response, &key)
    }

    /// 스트리밍 요청(`stream: true`)을 중계한다. 상류가 오류 상태로 답하면 상태만 담은 고정 본문 하나를 돌려준다.
    pub async fn forward_stream(&self, req: ModelRequest) -> Result<ModelStream, Error> {
        let Prepared { slot, request, key } = self.prepare(req, true).await?;
        let upstream = timeout(self.limits.timeout, self.upstream.stream(request))
            .await
            .map_err(|_| Error::Unavailable)??;
        Ok(ModelStream::new(upstream, key, self.limits, slot))
    }

    /// 요청 본문: 엄격히 읽고, 상류 모델 이름으로 덮고, 스트리밍 여부를 요청 종류와 맞추고, 정규화한다.
    fn request_body(
        &self,
        body: &JsonPayload,
        model: &str,
        streaming: bool,
        extra: &[String],
    ) -> Result<String, Error> {
        if body.0.len() > self.limits.max_request {
            return Err(Error::Invalid);
        }
        let Some(Value::Object(mut map)) = parse(&body.0) else {
            return Err(Error::Invalid);
        };
        let wants_stream = match map.get("stream") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err(Error::Invalid),
        };
        if wants_stream != streaming {
            return Err(Error::Invalid);
        }
        vet_request(&map, extra)?;
        map.insert("model".into(), Value::String(model.to_owned()));
        canonicalize(&Value::Object(map)).ok_or(Error::Invalid)
    }

    /// 응답: 크기·형식·키 노출을 검사한다. 상류 오류는 상태만 남긴다.
    fn response_body(
        &self,
        response: UpstreamResponse,
        key: &[u8],
    ) -> Result<ModelResponse, Error> {
        if response.body.len() > self.limits.max_response {
            return Err(Error::Unavailable);
        }
        if contains_credential(&response.body, &[key.to_vec()]) {
            return Err(Error::Denied);
        }
        if !(200..300).contains(&response.status) {
            let status = response.status;
            return Ok(ModelResponse {
                status,
                body: JsonPayload(format!("{{\"error\":{{\"status\":{status}}}}}")),
            });
        }
        let text = String::from_utf8(response.body).map_err(|_| Error::Unavailable)?;
        let decoded = parse(&text).ok_or(Error::Unavailable)?;
        // 이스케이프(`p`…)로 쓴 키는 원문 바이트에 보이지 않으므로 디코딩한 키·문자열 값도 검사한다.
        if decoded_leaks(&decoded, key) {
            return Err(Error::Denied);
        }
        Ok(ModelResponse {
            status: response.status,
            body: JsonPayload(text),
        })
    }
}

/// 디코딩한 JSON의 모든 키와 문자열 값에 키가 들어 있는가.
pub(crate) fn decoded_leaks(value: &Value, key: &[u8]) -> bool {
    let hit = |text: &str| contains_credential(text.as_bytes(), &[key.to_vec()]);
    match value {
        Value::String(text) => hit(text),
        Value::Array(items) => items.iter().any(|item| decoded_leaks(item, key)),
        Value::Object(map) => map
            .iter()
            .any(|(name, item)| hit(name) || decoded_leaks(item, key)),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
