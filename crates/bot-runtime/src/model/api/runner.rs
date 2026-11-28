//! Direct HTTP API model runner (`RunnerKind::Step`) with double activation fencing and secret response scanning.

use super::{
    endpoint::ApiEndpoint,
    scan::Needles,
    transport::ApiClient,
    wire::{self, ApiKind, Built, fail},
};
use crate::model::{
    ModelError, ModelMeta, ModelRequest, ModelRunner, ModelStep, RunnerKind, StepFuture,
    driver::cancelled,
};
use bot_core::{
    active::ActiveCheck,
    auth::{
        AuthError, AuthResolver, AuthSource, CallerSurface, HeaderMode, ResolvedAuth,
        StaticAuthResolver,
    },
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

/// Minimum length for a provider key so response leak scanning avoids false positives.
const MIN_KEY_BYTES: usize = 8;
const GATE_WAIT: Duration = Duration::from_secs(10);

/// Token limit field name for OpenAI-compatible requests (Anthropic always uses `max_tokens`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LimitField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl LimitField {
    pub fn name(self) -> &'static str {
        match self {
            LimitField::MaxTokens => "max_tokens",
            LimitField::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Reasoning effort level mapped per protocol (`--effort`, `output_config.effort`, `reasoning_effort`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct ApiSettings {
    pub kind: ApiKind,
    pub endpoint: ApiEndpoint,
    pub model: String,
    /// Profile secret name; `None` is allowed only for loopback endpoints or when `auth` is set.
    pub secret: Option<SecretName>,
    /// Managed host credential source (mutually exclusive with `secret`; takes precedence if set).
    pub auth: Option<AuthSource>,
    pub max_tokens: Option<u32>,
    pub limit_field: LimitField,
    pub effort: Option<Effort>,
}

impl ApiSettings {
    pub(crate) fn auth_source(&self) -> Option<AuthSource> {
        self.auth
            .clone()
            .or_else(|| self.secret.clone().map(AuthSource::Secret))
    }

    /// Provider fingerprint bound to opaque turn state so provider-specific IDs/signatures replay only to the same target.
    pub(crate) fn fingerprint(&self) -> String {
        let cred = match self.auth_source() {
            Some(AuthSource::Secret(name)) => name.as_str().to_owned(),
            Some(AuthSource::Credential {
                name,
                expected_kind,
            }) => format!("{}:{}", expected_kind.as_str(), name.as_str()),
            None => String::new(),
        };
        format!(
            "{:?}|{}|{}|{cred}",
            self.kind,
            self.endpoint.url(""),
            self.model,
        )
    }
}

pub struct ApiRunner {
    settings: ApiSettings,
    auth: Arc<dyn AuthResolver>,
    gate: Arc<dyn ActiveCheck>,
    client: ApiClient,
}

impl ApiRunner {
    pub fn new<S: SecretStore + 'static>(
        settings: ApiSettings,
        secrets: Arc<S>,
        gate: Arc<dyn ActiveCheck>,
    ) -> Result<Self, String> {
        Self::with_resolver(settings, Arc::new(StaticAuthResolver::new(secrets)), gate)
    }

    pub fn with_resolver(
        settings: ApiSettings,
        auth: Arc<dyn AuthResolver>,
        gate: Arc<dyn ActiveCheck>,
    ) -> Result<Self, String> {
        if settings.auth_source().is_none() && !settings.endpoint.is_loopback() {
            return Err("only loopback endpoints may omit a provider secret".to_owned());
        }
        if settings.model.trim().is_empty() {
            return Err("model ID cannot be empty".to_owned());
        }
        let client = ApiClient::new(settings.endpoint.is_loopback())?;
        Ok(Self {
            settings,
            auth,
            gate,
            client,
        })
    }

    fn header_mode(&self, source: &AuthSource) -> HeaderMode {
        match (self.settings.kind, source) {
            (ApiKind::Anthropic, AuthSource::Secret(_)) => HeaderMode::ApiKey("x-api-key"),
            (ApiKind::Anthropic, AuthSource::Credential { .. }) | (ApiKind::OpenAi, _) => {
                HeaderMode::Bearer
            }
        }
    }

    async fn resolve_auth(
        &self,
        profile: &ProfileId,
        source: AuthSource,
    ) -> Result<(String, Vec<(String, String)>, Vec<Vec<u8>>), ModelError> {
        let resolved: ResolvedAuth = self
            .auth
            .resolve(profile.clone(), source, CallerSurface::DirectApi, false)
            .await
            .map_err(|error| match error {
                AuthError::Unavailable => fail("secret_unavailable", true),
                AuthError::Stale => fail("stale_worker", true),
                AuthError::NeedsLogin => fail("credential_needs_login", false),
                AuthError::Denied => fail("credential_denied", false),
                AuthError::Conflict => fail("credential_conflict", true),
                AuthError::Invalid => fail("model_key_invalid", false),
                AuthError::Missing => fail("model_key_missing", false),
            })?;
        let scan = resolved.scan_values();
        let text = std::str::from_utf8(&resolved.secret.0)
            .map_err(|_| fail("model_key_invalid", false))?
            .trim()
            .to_owned();
        // Require visible ASCII only to prevent header splitting.
        if text.len() < MIN_KEY_BYTES || !text.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(fail("model_key_invalid", false));
        }
        Ok((text, resolved.headers, scan))
    }

    /// Confirms this process still owns the active set within `GATE_WAIT` (same deadline rule as `GuardedUpstream::confirm`).
    async fn confirm(&self) -> Result<(), ModelError> {
        let limit = Instant::now() + GATE_WAIT;
        match timeout_at(limit, self.gate.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            Ok(Err(bot_core::port::Error::Stale)) => Err(fail("stale_worker", true)),
            _ => Err(fail("activation_unverified", true)),
        }
    }

    /// Double-checks activation before reading auth and again right before POST so stale workers neither read nor transmit keys.
    async fn send(&self, request: &ModelRequest, body: Vec<u8>) -> Result<Sent, ModelError> {
        self.confirm().await?;
        let (mode, key, extra, scan) = match self.settings.auth_source() {
            Some(source) => {
                let mode = self.header_mode(&source);
                let (key, extra, scan) =
                    self.resolve_auth(&request.attempt.profile, source).await?;
                self.confirm().await?;
                (Some(mode), Some(key), extra, scan)
            }
            None => (None, None, Vec::new(), Vec::new()),
        };
        let headers = self.settings.kind.headers(mode, key.as_deref(), &extra);
        let url = self.settings.endpoint.url(self.settings.kind.path());
        match self
            .client
            .post(&url, headers, body, request.limits.idle)
            .await
        {
            Ok(answer) => Ok(Sent {
                key,
                scan,
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
        // Direct API runners provide no host file built-ins; fail closed before contacting upstream.
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
                    "upstream model returned error status"
                );
            }
            return Err(error);
        }
        let text = std::str::from_utf8(&sent.body).map_err(|_| wire::invalid())?;
        // Reject the entire response if any resolved credential appears in raw or decoded output.
        let mut needles: Vec<Needles> = sent.key.as_deref().map(Needles::new).into_iter().collect();
        for bytes in &sent.scan {
            if let Ok(text) = std::str::from_utf8(bytes)
                && Some(text) != sent.key.as_deref()
            {
                needles.push(Needles::new(text));
            }
        }
        if needles.iter().any(|n| n.in_body(text)) {
            return Err(fail("secret_in_response", false));
        }
        let step = self
            .settings
            .kind
            .parse(&self.settings, &sent.body, &request, &names)?;
        if needles.iter().any(|n| n.in_step(&step)) {
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

    fn meta(&self, _alias: &str) -> Option<ModelMeta> {
        let provider = match self.settings.kind {
            ApiKind::Anthropic => "anthropic",
            ApiKind::OpenAi => "openai",
        };
        Some(ModelMeta {
            provider,
            model: self.settings.model.clone(),
        })
    }
}

struct Sent {
    key: Option<String>,
    scan: Vec<Vec<u8>>,
    status: u16,
    body: Vec<u8>,
}

fn status_error(status: u16, body: &[u8]) -> Option<ModelError> {
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

/// Extracts only allowlisted error codes for logging so raw upstream error bodies are never emitted.
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
    use super::{ApiClient, ModelError, known_error_type, status_error};
    use crate::model::api::transport::ERROR_BODY_MAX;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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

    async fn serve_once(status: &str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req).await;
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        format!("http://{addr}/v1/messages")
    }

    #[tokio::test]
    async fn non_2xx_error_body_is_read_bounded_and_classified() {
        let client = ApiClient::new(true).unwrap();
        let url = serve_once(
            "400 Bad Request",
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"sensitive-prompt"}}"#.to_vec(),
        )
        .await;
        let answer = client
            .post(&url, vec![], b"{}".to_vec(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer.status, 400);
        assert_eq!(
            status_error(answer.status, &answer.body),
            Some(ModelError::Failed {
                code: "upstream_rejected",
                transient: false,
            })
        );
        assert_eq!(known_error_type(&answer.body), "invalid_request_error");

        let mut big = vec![b' '; ERROR_BODY_MAX + 4096];
        big.extend_from_slice(br#"{"error":{"type":"invalid_request_error"}}"#);
        let url = serve_once("400 Bad Request", big).await;
        let answer = client
            .post(&url, vec![], b"{}".to_vec(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer.status, 400);
        assert_eq!(answer.body.len(), ERROR_BODY_MAX);
        assert_eq!(known_error_type(&answer.body), "unknown");
    }
}
