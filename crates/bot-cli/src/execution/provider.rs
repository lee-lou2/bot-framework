//! Model provider parsing, validation, display, and authority hashing for `host.json`.
use super::{field, object, only, text};
use bot_core::id::SecretName;
use bot_runtime::model::{ApiEndpoint, ApiKind, Effort, LimitField};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    Demo,
    ClaudeCli {
        model: String,
        endpoint: String,
        secret: String,
        effort: Option<Effort>,
    },
    Api(ApiProvider),
}

/// Direct API provider config (`anthropic-api` / `openai-api`); keeps raw endpoint text for authority hashing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiProvider {
    pub kind: ApiKind,
    pub model: String,
    pub endpoint: String,
    /// Profile secret name; may be omitted only for loopback endpoints.
    pub secret: Option<String>,
    pub max_tokens: Option<u32>,
    /// Upstream field name (`max_tokens` or OpenAI-only `max_completion_tokens`).
    pub limit_field: LimitField,
    pub effort: Option<Effort>,
}

impl Provider {
    pub fn secret(&self) -> Option<&str> {
        match self {
            Provider::Demo => None,
            Provider::ClaudeCli { secret, .. } => Some(secret),
            Provider::Api(api) => api.secret.as_deref(),
        }
    }

    pub fn needs_model_vm(&self) -> bool {
        match self {
            Provider::Demo | Provider::Api(_) => false,
            Provider::ClaudeCli { .. } => true,
        }
    }

    /// Operator summary line: exposes only secret names (never values) and upstream host (never path/query).
    pub fn describe(&self, alias: &str) -> String {
        match self {
            Provider::Demo => format!("{alias}: demo (local demo model, no external calls)"),
            Provider::ClaudeCli {
                model,
                endpoint,
                secret,
                effort,
            } => {
                let effort_note = effort.map_or_else(String::new, |e| format!(" · effort {e}"));
                format!(
                    "{alias}: claude-cli · model {model} · upstream {} · secret {secret}{effort_note}",
                    upstream_host(endpoint)
                )
            }
            Provider::Api(api) => {
                let kind = match api.kind {
                    ApiKind::Anthropic => "anthropic-api",
                    ApiKind::OpenAi => "openai-api",
                };
                let key = api.secret.as_deref().map_or_else(
                    || "no service key (loopback only)".to_owned(),
                    |name| format!("secret {name}"),
                );
                let limit =
                    api.max_tokens
                        .map_or_else(String::new, |tokens| match api.limit_field {
                            LimitField::MaxTokens => format!(" · max tokens {tokens}"),
                            other => format!(" · max tokens {tokens} ({})", other.name()),
                        });
                let effort_note = api
                    .effort
                    .map_or_else(String::new, |e| format!(" · effort {e}"));
                format!(
                    "{alias}: {kind} · model {} · upstream {} · {key}{limit}{effort_note}",
                    api.model,
                    upstream_host(&api.endpoint)
                )
            }
        }
    }

    /// Hashes authority-relevant fields into the activation fingerprint (byte-pinned by `fingerprint_tests.rs`).
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
            // Output limits and effort are operational tuning, not authority. Destructure every field explicitly so new fields force a review.
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

fn upstream_host(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// Validates `claude-cli` upstream URLs with the same `HostPort::parse` rules as broker egress; rejects alternative IP encodings (`0x`, numeric final label).
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
    let raw = value
        .as_str()
        .ok_or_else(|| format!("{path}.effort: must be one of low, medium, high, xhigh, max"))?;
    Effort::parse(raw)
        .map(Some)
        .ok_or_else(|| format!("{path}.effort: must be one of low, medium, high, xhigh, max"))
}

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
                return Err(format!("{path}.secret: invalid secret name"));
            }
            Some(name.to_owned())
        }
    };
    if secret.is_none() && !endpoint.is_loopback() {
        return Err(format!(
            "{path}.secret: service key secret name is required (omitting secret is only allowed for loopback IP addresses)"
        ));
    }
    if fields.contains_key("max_tokens") && fields.contains_key("max_completion_tokens") {
        return Err(format!(
            "{path}: cannot set both max_tokens and max_completion_tokens"
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
                .ok_or_else(|| {
                    format!("{path}.{name}: must be an integer between 1 and 1000000")
                })?,
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

pub fn parse_providers(value: &Value) -> Result<BTreeMap<String, Provider>, String> {
    let mut out = BTreeMap::new();
    for (alias, entry) in object(value, "providers")? {
        let path = format!("providers.{alias}");
        if !bot_core::id::is_name(alias) {
            return Err(format!(
                "{path}: alias must follow naming rules (lowercase letters, digits, hyphens)"
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
                        "{path}.endpoint: must be `https://host[:port][/path]` (no credentials, query, or fragment)"
                    ));
                }
                let secret = text(fields, "secret", &path)?;
                if SecretName::parse(secret).is_none() {
                    return Err(format!("{path}.secret: invalid secret name"));
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
                    "{path}.kind: pi is not supported until an isolation and resume adapter is available"
                ));
            }
            other => {
                return Err(format!(
                    "{path}.kind: unsupported kind `{other}` (expected demo, claude-cli, anthropic-api, or openai-api)"
                ));
            }
        };
        out.insert(alias.clone(), provider);
    }
    Ok(out)
}
