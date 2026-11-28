//! Model provider parsing and validation for `host.json`.
use super::{object, only, text};
use bot_core::{
    auth::{AuthSource, CredentialKind},
    id::SecretName,
};
use bot_runtime::model::{ApiEndpoint, ApiKind, CliKind, Effort, LimitField};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_FALLBACK_DEPTH: usize = 8;

/// Host-managed credential reference (`oauth`, `service-account`, or `subscription-token`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderAuth {
    pub kind: CredentialKind,
    pub credential: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provider {
    Demo,
    Cli(CliProvider),
    Api(ApiProvider),
}

/// VM-hosted CLI provider config (`claude-cli`, `codex-cli`, `agy-cli`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliProvider {
    pub kind: CliKind,
    pub model: String,
    pub endpoint: String,
    /// Static profile secret name (mutually exclusive with `auth`).
    pub secret: Option<String>,
    /// Managed host credential reference (mutually exclusive with `secret`).
    pub auth: Option<ProviderAuth>,
    /// Optional output-token cap (`claude-cli` only; operational tuning, not authority).
    pub max_tokens: Option<u32>,
    pub effort: Option<Effort>,
    pub fallback: Option<String>,
}

/// Direct API provider config (`anthropic-api` / `openai-api`); keeps raw endpoint text for authority hashing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiProvider {
    pub kind: ApiKind,
    pub model: String,
    pub endpoint: String,
    /// Profile secret name; may be omitted only for loopback endpoints or when `auth` is set.
    pub secret: Option<String>,
    /// Managed host credential reference (`oauth` or `service-account`; mutually exclusive with `secret`).
    pub auth: Option<ProviderAuth>,
    pub max_tokens: Option<u32>,
    /// Upstream field name (`max_tokens` or OpenAI-only `max_completion_tokens`).
    pub limit_field: LimitField,
    pub effort: Option<Effort>,
    pub fallback: Option<String>,
}

impl Provider {
    pub fn secret(&self) -> Option<&str> {
        match self {
            Provider::Demo => None,
            Provider::Cli(cli) => cli.secret.as_deref(),
            Provider::Api(api) => api.secret.as_deref(),
        }
    }

    pub fn auth(&self) -> Option<&ProviderAuth> {
        match self {
            Provider::Demo => None,
            Provider::Cli(cli) => cli.auth.as_ref(),
            Provider::Api(api) => api.auth.as_ref(),
        }
    }

    pub fn auth_source(&self, alias: &str) -> Result<Option<AuthSource>, String> {
        if let Some(auth) = self.auth() {
            let name = SecretName::parse(&auth.credential).ok_or_else(|| {
                format!("providers.{alias}.auth.credential is not a valid credential name")
            })?;
            return Ok(Some(AuthSource::Credential {
                name,
                expected_kind: auth.kind,
            }));
        }
        match self.secret() {
            Some(secret) => {
                let name = SecretName::parse(secret).ok_or_else(|| {
                    format!("providers.{alias}.secret is not a valid secret name")
                })?;
                Ok(Some(AuthSource::Secret(name)))
            }
            None => Ok(None),
        }
    }

    pub fn fallback(&self) -> Option<&str> {
        match self {
            Provider::Demo => None,
            Provider::Cli(cli) => cli.fallback.as_deref(),
            Provider::Api(api) => api.fallback.as_deref(),
        }
    }

    pub fn needs_model_vm(&self) -> bool {
        matches!(self, Provider::Cli(_))
    }

    pub fn cli_kind(&self) -> Option<CliKind> {
        match self {
            Provider::Cli(cli) => Some(cli.kind),
            Provider::Demo | Provider::Api(_) => None,
        }
    }
}

/// Validates VM CLI (`claude-cli`, `codex-cli`, `agy-cli`) upstream URLs with the same `HostPort::parse` rules as broker egress; rejects alternative IP encodings (`0x`, numeric final label).
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

fn parse_fallback(fields: &Map<String, Value>, path: &str) -> Result<Option<String>, String> {
    if !fields.contains_key("fallback") {
        return Ok(None);
    }
    let raw = text(fields, "fallback", path)?;
    if !bot_core::id::is_name(raw) {
        return Err(format!(
            "{path}.fallback: must follow naming rules (lowercase letters, digits, hyphens)"
        ));
    }
    Ok(Some(raw.to_owned()))
}

fn parse_secret(fields: &Map<String, Value>, path: &str) -> Result<Option<String>, String> {
    let Some(_) = fields.get("secret") else {
        return Ok(None);
    };
    let name = text(fields, "secret", path)?;
    SecretName::parse(name)
        .map(|_| Some(name.to_owned()))
        .ok_or_else(|| format!("{path}.secret: invalid secret name"))
}

fn parse_auth(
    fields: &Map<String, Value>,
    path: &str,
    allow_sub: bool,
) -> Result<Option<ProviderAuth>, String> {
    let Some(value) = fields.get("auth") else {
        return Ok(None);
    };
    let p = format!("{path}.auth");
    let map = object(value, &p)?;
    only(map, &["kind", "credential"], &p)?;
    let kind = match text(map, "kind", &p)? {
        "oauth" => CredentialKind::OAuth,
        "service-account" => CredentialKind::ServiceAccount,
        "subscription-token" if allow_sub => CredentialKind::SubscriptionToken,
        "subscription-token" => {
            return Err(format!(
                "{p}.kind: subscription-token is permitted only on `claude-cli`"
            ));
        }
        other => {
            let want = if allow_sub {
                "oauth, service-account, or subscription-token"
            } else {
                "oauth or service-account"
            };
            return Err(format!(
                "{p}.kind: unsupported auth kind `{other}` (expected {want})"
            ));
        }
    };
    let cred = text(map, "credential", &p)?;
    let parsed = SecretName::parse(cred)
        .filter(|n| !bot_broker::private::reserved_secret(n))
        .ok_or_else(|| format!("{p}.credential: invalid or reserved credential name"))?;
    Ok(Some(ProviderAuth {
        kind,
        credential: parsed.as_str().to_owned(),
    }))
}

fn parse_max_tokens(
    fields: &Map<String, Value>,
    path: &str,
    field_name: &str,
) -> Result<Option<u32>, String> {
    match fields.get(field_name) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|tokens| (1..=1_000_000).contains(tokens))
            .and_then(|tokens| u32::try_from(tokens).ok())
            .map(Some)
            .ok_or_else(|| {
                format!("{path}.{field_name}: must be an integer between 1 and 1000000")
            }),
    }
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
            "auth",
            "max_tokens",
            "effort",
            "fallback",
        ],
        ApiKind::OpenAi => &[
            "kind",
            "model",
            "endpoint",
            "secret",
            "auth",
            "max_tokens",
            "max_completion_tokens",
            "effort",
            "fallback",
        ],
    };
    only(fields, allowed, path)?;
    let model = text(fields, "model", path)?;
    let endpoint_text = text(fields, "endpoint", path)?;
    let endpoint =
        ApiEndpoint::parse(endpoint_text).map_err(|reason| format!("{path}.endpoint: {reason}"))?;
    let secret = parse_secret(fields, path)?;
    let auth = parse_auth(fields, path, false)?;
    if secret.is_some() && auth.is_some() {
        return Err(format!("{path}: cannot set both `secret` and `auth`"));
    }
    if secret.is_none() && auth.is_none() && !endpoint.is_loopback() {
        return Err(format!(
            "{path}.secret: service key secret name or `auth` is required (omitting both is only allowed for loopback IP addresses)"
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
    Ok(ApiProvider {
        kind,
        model: model.to_owned(),
        endpoint: endpoint_text.to_owned(),
        secret,
        auth,
        max_tokens: parse_max_tokens(fields, path, name)?,
        limit_field,
        effort: parse_effort(fields, path)?,
        fallback: parse_fallback(fields, path)?,
    })
}

fn parse_cli(
    fields: &Map<String, Value>,
    path: &str,
    kind: CliKind,
) -> Result<CliProvider, String> {
    let allowed: &[&str] = match kind {
        CliKind::Claude => &[
            "kind",
            "model",
            "endpoint",
            "secret",
            "auth",
            "max_tokens",
            "effort",
            "fallback",
        ],
        CliKind::Codex | CliKind::Agy => &[
            "kind", "model", "endpoint", "secret", "auth", "effort", "fallback",
        ],
    };
    only(fields, allowed, path)?;
    let endpoint = text(fields, "endpoint", path)?;
    if !endpoint_ok(endpoint) {
        return Err(format!(
            "{path}.endpoint: must be `https://host[:port][/path]` (no credentials, query, or fragment)"
        ));
    }
    let secret = parse_secret(fields, path)?;
    let auth = parse_auth(fields, path, kind == CliKind::Claude)?;
    match (&secret, &auth) {
        (Some(_), Some(_)) => return Err(format!("{path}: cannot set both `secret` and `auth`")),
        (None, None) => return Err(format!("{path}: either `secret` or `auth` is required")),
        _ => {}
    }
    if let Some(auth) = &auth
        && auth.kind == CredentialKind::SubscriptionToken
    {
        let rest = endpoint.strip_prefix("https://").unwrap_or("");
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.split(':').next().unwrap_or("");
        if !host.eq_ignore_ascii_case("api.anthropic.com") {
            return Err(format!(
                "{path}.endpoint: `subscription-token` auth is restricted to first-party `https://api.anthropic.com`"
            ));
        }
    }
    Ok(CliProvider {
        kind,
        model: text(fields, "model", path)?.to_owned(),
        endpoint: endpoint.to_owned(),
        secret,
        auth,
        max_tokens: if kind == CliKind::Claude {
            parse_max_tokens(fields, path, "max_tokens")?
        } else {
            None
        },
        effort: parse_effort(fields, path)?,
        fallback: parse_fallback(fields, path)?,
    })
}

fn validate_fallbacks(providers: &BTreeMap<String, Provider>) -> Result<(), String> {
    for start in providers.keys() {
        let mut seen = BTreeSet::from([start.as_str()]);
        let mut current = start.as_str();
        let mut depth = 0usize;
        while let Some(next) = providers.get(current).and_then(Provider::fallback) {
            if !providers.contains_key(next) {
                return Err(format!(
                    "providers.{current}.fallback: unknown provider alias `{next}`"
                ));
            }
            if !seen.insert(next) {
                return Err(format!(
                    "providers.{start}.fallback: cycle detected at `{next}`"
                ));
            }
            depth += 1;
            if depth > MAX_FALLBACK_DEPTH {
                return Err(format!(
                    "providers.{start}.fallback: fallback chain exceeds maximum depth of {MAX_FALLBACK_DEPTH}"
                ));
            }
            current = next;
        }
    }
    Ok(())
}

/// Ordered fallback aliases reachable from `alias` (excluding `alias` itself).
pub fn fallback_chain(providers: &BTreeMap<String, Provider>, alias: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::from([alias]);
    let mut current = alias;
    while out.len() < MAX_FALLBACK_DEPTH
        && let Some(next) = providers.get(current).and_then(Provider::fallback)
        && providers.contains_key(next)
        && seen.insert(next)
    {
        out.push(next.to_owned());
        current = next;
    }
    out
}

/// All aliases in `roots` plus every alias reachable via `fallback` links.
pub fn reachable_aliases<'a>(
    providers: &BTreeMap<String, Provider>,
    roots: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<String> {
    let mut all = BTreeSet::new();
    for root in roots {
        all.insert(root.to_owned());
        all.extend(fallback_chain(providers, root));
    }
    all
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
            "claude-cli" => Provider::Cli(parse_cli(fields, &path, CliKind::Claude)?),
            "codex-cli" => Provider::Cli(parse_cli(fields, &path, CliKind::Codex)?),
            "agy-cli" => Provider::Cli(parse_cli(fields, &path, CliKind::Agy)?),
            "anthropic-api" => Provider::Api(parse_api(fields, &path, ApiKind::Anthropic)?),
            "openai-api" => Provider::Api(parse_api(fields, &path, ApiKind::OpenAi)?),
            "pi" => {
                return Err(format!(
                    "{path}.kind: pi is not supported until an isolation and resume adapter is available"
                ));
            }
            other => {
                return Err(format!(
                    "{path}.kind: unsupported kind `{other}` (expected demo, claude-cli, codex-cli, agy-cli, anthropic-api, or openai-api)"
                ));
            }
        };
        out.insert(alias.clone(), provider);
    }
    validate_fallbacks(&out)?;
    Ok(out)
}
