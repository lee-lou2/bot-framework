//! Host-owned credential vocabulary, header attachment rules, and cross-crate auth resolver and store ports.
use crate::{
    id::{ProfileId, SecretName},
    port::{Error, Reply, SecretStore, SecretValue},
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// Model CLI vendor surface used for host-side credential policy enforcement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CliVendor {
    Claude,
    Codex,
    Google,
}

impl CliVendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Google => "google",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "google" => Some(Self::Google),
            _ => None,
        }
    }
}

/// Trusted host caller surface requesting resolved authorization material.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallerSurface {
    DirectApi,
    CliRelay(CliVendor),
    RemoteMcp,
}

/// Kind of host-managed credential persisted in `bot_credentials`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum CredentialKind {
    OAuth,
    ServiceAccount,
    SubscriptionToken,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ServiceAccount => "service_account",
            Self::SubscriptionToken => "subscription_token",
        }
    }

    pub fn config_str(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ServiceAccount => "service-account",
            Self::SubscriptionToken => "subscription-token",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "oauth" => Some(Self::OAuth),
            "service_account" | "service-account" => Some(Self::ServiceAccount),
            "subscription_token" | "subscription-token" => Some(Self::SubscriptionToken),
            _ => None,
        }
    }
}

/// Lifecycle state of a persisted managed credential row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialState {
    Active,
    Refreshing,
    NeedsLogin,
}

impl CredentialState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Refreshing => "refreshing",
            Self::NeedsLogin => "needs_login",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "refreshing" => Some(Self::Refreshing),
            "needs_login" => Some(Self::NeedsLogin),
            _ => None,
        }
    }
}

/// Reference to either a static profile secret or a managed credential with an expected kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthSource {
    Secret(SecretName),
    Credential {
        name: SecretName,
        expected_kind: CredentialKind,
    },
}

impl AuthSource {
    pub fn credential_name(&self) -> &SecretName {
        match self {
            Self::Secret(name) | Self::Credential { name, .. } => name,
        }
    }

    pub fn is_managed(&self) -> bool {
        matches!(self, Self::Credential { .. })
    }
}

/// Fixed-vocabulary auth resolution errors; never carry credential bytes or upstream bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthError {
    Missing,
    Invalid,
    Denied,
    NeedsLogin,
    Stale,
    Unavailable,
    Conflict,
}

impl AuthError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Missing => "credential_missing",
            Self::Invalid => "credential_invalid",
            Self::Denied => "credential_denied",
            Self::NeedsLogin => "credential_needs_login",
            Self::Stale => "stale_worker",
            Self::Unavailable => "secret_unavailable",
            Self::Conflict => "credential_conflict",
        }
    }
}

impl From<Error> for AuthError {
    fn from(error: Error) -> Self {
        match error {
            Error::Missing => Self::Missing,
            Error::Invalid => Self::Invalid,
            Error::Denied => Self::Denied,
            Error::Stale => Self::Stale,
            Error::Conflict => Self::Conflict,
            Error::Unavailable | Error::RateLimited => Self::Unavailable,
        }
    }
}

/// Flat resolved host-only authorization material; callers never inspect credential kinds.
pub struct ResolvedAuth {
    pub secret: SecretValue,
    /// Additional allowlisted non-secret headers (e.g. account/project identifiers) attached on the host.
    pub headers: Vec<(String, String)>,
}

impl ResolvedAuth {
    pub fn plain(secret: SecretValue) -> Self {
        Self {
            secret,
            headers: Vec::new(),
        }
    }

    /// Returns secret bytes to scan for leaks in upstream responses and streams.
    pub fn scan_values(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(1);
        if !self.secret.0.is_empty() {
            out.push(self.secret.0.clone());
        }
        out
    }
}

/// Host header attachment rule for outbound model and remote MCP requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderMode {
    ApiKey(&'static str),
    Bearer,
    /// Sends `Authorization: Bearer <secret>` and ensures `token` is present in comma-separated `header`.
    BearerMergeHeader {
        header: &'static str,
        token: &'static str,
    },
}

/// Protected auth header names that untrusted sandbox/extra headers may never inject or override.
pub fn is_auth_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("x-api-key")
        || name.eq_ignore_ascii_case("x-goog-api-key")
        || name.eq_ignore_ascii_case("proxy-authorization")
}

fn header_value_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

fn merge_csv_tokens(existing: &[String], required: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for raw in existing {
        for item in raw.split(',') {
            let trimmed = item.trim();
            if !trimmed.is_empty() && !parts.iter().any(|p| p.eq_ignore_ascii_case(trimmed)) {
                parts.push(trimmed.to_owned());
            }
        }
    }
    if !parts.iter().any(|p| p.eq_ignore_ascii_case(required)) {
        parts.push(required.to_owned());
    }
    parts.join(",")
}

/// Builds outbound headers with host auth attachment, fixed headers, extra non-secret headers,
/// and allowlisted forwarded headers (merging comma-separated tokens for `BearerMergeHeader`).
pub fn apply_auth_headers(
    mode: HeaderMode,
    secret: &str,
    extra_headers: &[(String, String)],
    fixed_headers: &[(String, String)],
    forwarded_headers: &[(String, String)],
    allow_forward: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    match mode {
        HeaderMode::ApiKey(name) => out.push((name.to_owned(), secret.to_owned())),
        HeaderMode::Bearer | HeaderMode::BearerMergeHeader { .. } => {
            out.push(("authorization".to_owned(), format!("Bearer {secret}")));
        }
    }
    out.extend(fixed_headers.iter().cloned());
    for (name, value) in extra_headers {
        let lower = name.to_ascii_lowercase();
        if !is_auth_header(&lower)
            && header_value_ok(value)
            && !out.iter().any(|(k, _)| k.eq_ignore_ascii_case(&lower))
        {
            out.push((lower, value.clone()));
        }
    }
    let merge_target = match mode {
        HeaderMode::BearerMergeHeader { header, token } => Some((header, token)),
        HeaderMode::ApiKey(_) | HeaderMode::Bearer => None,
    };
    let mut merged_values = Vec::new();
    for (name, value) in forwarded_headers {
        let lower = name.to_ascii_lowercase();
        if is_auth_header(&lower) || !allow_forward.iter().any(|a| a.eq_ignore_ascii_case(&lower)) {
            continue;
        }
        if !header_value_ok(value) {
            continue;
        }
        if let Some((merge_header, _)) = merge_target
            && lower.eq_ignore_ascii_case(merge_header)
        {
            merged_values.push(value.clone());
            continue;
        }
        if !out.iter().any(|(k, _)| k.eq_ignore_ascii_case(&lower)) {
            out.push((lower, value.clone()));
        }
    }
    if let Some((merge_header, required_token)) = merge_target {
        let combined = merge_csv_tokens(&merged_values, required_token);
        out.retain(|(k, _)| !k.eq_ignore_ascii_case(merge_header));
        out.push((merge_header.to_ascii_lowercase(), combined));
    }
    out
}

pub type AuthReply<'a, T> = Pin<Box<dyn Future<Output = Result<T, AuthError>> + Send + 'a>>;

/// Trusted host port resolving static secrets or managed credentials into flat outbound auth.
pub trait AuthResolver: Send + Sync {
    fn resolve(
        &self,
        profile: ProfileId,
        source: AuthSource,
        surface: CallerSurface,
        force_refresh: bool,
    ) -> AuthReply<'_, ResolvedAuth>;
}

/// Compatibility resolver serving static profile secrets through `SecretStore`.
pub struct StaticAuthResolver {
    secrets: Arc<dyn SecretStore>,
}

impl StaticAuthResolver {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self { secrets }
    }
}

impl AuthResolver for StaticAuthResolver {
    fn resolve(
        &self,
        profile: ProfileId,
        source: AuthSource,
        _surface: CallerSurface,
        _force_refresh: bool,
    ) -> AuthReply<'_, ResolvedAuth> {
        Box::pin(async move {
            let AuthSource::Secret(name) = source else {
                return Err(AuthError::Unavailable);
            };
            let handle = self
                .secrets
                .for_profile(profile)
                .await
                .map_err(AuthError::from)?;
            let secret = handle.resolve(name).await.map_err(AuthError::from)?;
            Ok(ResolvedAuth::plain(secret))
        })
    }
}

/// Encrypted row persisted in PostgreSQL `bot_credentials`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredCredential {
    pub profile: ProfileId,
    pub name: SecretName,
    pub kind: CredentialKind,
    /// Exact canonical `host:port` of the token endpoint (empty for non-refreshing credentials).
    pub token_endpoint_host: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub version: i64,
    pub state: CredentialState,
    pub refresh_nonce: Option<String>,
    pub refresh_until_ms: Option<i64>,
    pub expires_ms: Option<i64>,
    pub updated_ms: i64,
}

/// Write payload for creating or replacing an encrypted credential row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialUpsert {
    pub profile: ProfileId,
    pub name: SecretName,
    pub kind: CredentialKind,
    pub token_endpoint_host: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub expires_ms: Option<i64>,
}

/// Non-secret metadata view returned by status and doctor checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialSummary {
    pub profile: ProfileId,
    pub name: SecretName,
    pub kind: CredentialKind,
    pub token_endpoint_host: String,
    pub version: i64,
    pub state: CredentialState,
    pub expires_ms: Option<i64>,
    pub updated_ms: i64,
}

/// Dedicated-connection advisory lock guard exposing CAS refresh mutations on the same PG connection.
pub trait CredentialLease: Send {
    fn get(&mut self) -> Reply<'_, Option<StoredCredential>>;
    fn now_ms(&mut self) -> Reply<'_, i64>;
    fn begin_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        refresh_nonce: String,
        lease_ms: i64,
    ) -> Reply<'_, i64>;
    fn commit_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        next: CredentialUpsert,
    ) -> Reply<'_, i64>;
    fn mark_needs_login(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        reason: &'static str,
    ) -> Reply<'_, i64>;
    fn abort_refresh(
        &mut self,
        expected_version: i64,
        expected_nonce: Vec<u8>,
        reason: &'static str,
    ) -> Reply<'_, i64>;
    fn release(self: Box<Self>) -> Reply<'static, ()>;
}

/// Durable storage port for host-encrypted OAuth, service-account, and subscription credentials.
pub trait CredentialStore: Send + Sync {
    fn get_credential(
        &self,
        profile: ProfileId,
        name: SecretName,
    ) -> Reply<'_, Option<StoredCredential>>;
    fn list_credentials(&self, profile: Option<ProfileId>) -> Reply<'_, Vec<CredentialSummary>>;
    fn upsert_credential(&self, actor: String, entry: CredentialUpsert) -> Reply<'_, i64>;
    fn delete_credential(
        &self,
        actor: String,
        profile: ProfileId,
        name: SecretName,
    ) -> Reply<'_, bool>;
    fn lock_credential(
        &self,
        profile: ProfileId,
        name: SecretName,
        wait: Duration,
    ) -> Reply<'_, Box<dyn CredentialLease>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_merge_header_preserves_and_deduplicates_forwarded_tokens() {
        let headers = apply_auth_headers(
            HeaderMode::BearerMergeHeader {
                header: "anthropic-beta",
                token: "oauth-2025-04-20",
            },
            "tok-1",
            &[
                ("chatgpt-account-id".into(), "acct-1".into()),
                ("authorization".into(), "spoof".into()),
            ],
            &[("anthropic-version".into(), "2023-06-01".into())],
            &[
                (
                    "anthropic-beta".into(),
                    "claude-code-20250219,prompt-caching".into(),
                ),
                ("authorization".into(), "Bearer evil".into()),
                ("x-api-key".into(), "evil".into()),
            ],
            &["anthropic-beta".into(), "authorization".into()],
        );
        assert!(headers.contains(&("authorization".into(), "Bearer tok-1".into())));
        assert!(headers.contains(&("anthropic-version".into(), "2023-06-01".into())));
        assert!(headers.contains(&("chatgpt-account-id".into(), "acct-1".into())));
        let beta = headers
            .iter()
            .find(|(k, _)| k == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(beta, "claude-code-20250219,prompt-caching,oauth-2025-04-20");
        assert_eq!(
            headers.iter().filter(|(k, _)| k == "authorization").count(),
            1
        );
        assert!(!headers.iter().any(|(k, _)| k == "x-api-key"));
    }

    #[test]
    fn bearer_merge_header_does_not_duplicate_existing_required_token() {
        let headers = apply_auth_headers(
            HeaderMode::BearerMergeHeader {
                header: "anthropic-beta",
                token: "oauth-2025-04-20",
            },
            "tok-2",
            &[],
            &[],
            &[("Anthropic-Beta".into(), "OAUTH-2025-04-20, foo".into())],
            &["anthropic-beta".into()],
        );
        let beta = headers
            .iter()
            .find(|(k, _)| k == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(beta, "OAUTH-2025-04-20,foo");
    }

    #[test]
    fn credential_kinds_and_states_round_trip() {
        for kind in [
            CredentialKind::OAuth,
            CredentialKind::ServiceAccount,
            CredentialKind::SubscriptionToken,
        ] {
            assert_eq!(CredentialKind::parse(kind.as_str()), Some(kind));
            assert_eq!(CredentialKind::parse(kind.config_str()), Some(kind));
        }
        assert_eq!(CredentialKind::parse("static"), None);
        for state in [
            CredentialState::Active,
            CredentialState::Refreshing,
            CredentialState::NeedsLogin,
        ] {
            assert_eq!(CredentialState::parse(state.as_str()), Some(state));
        }
    }
}
