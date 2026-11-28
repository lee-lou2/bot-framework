//! Provider operator summaries and authority fingerprint hashing.
use super::field;
use super::provider::{ApiProvider, CliProvider, Provider};
use bot_runtime::model::{ApiKind, CliKind, LimitField};
use sha2::{Digest, Sha256};

impl Provider {
    /// Operator summary line: exposes only secret names (never values) and upstream host (never path/query).
    pub fn describe(&self, alias: &str) -> String {
        match self {
            Provider::Demo => format!("{alias}: demo (local demo model, no external calls)"),
            Provider::Cli(cli) => {
                let auth_note = match (&cli.secret, &cli.auth) {
                    (Some(name), _) => format!("secret {name}"),
                    (None, Some(auth)) => {
                        format!("auth {} {}", auth.kind.config_str(), auth.credential)
                    }
                    (None, None) => "no auth".to_owned(),
                };
                let limit_note = cli
                    .max_tokens
                    .map_or_else(String::new, |tokens| format!(" · max tokens {tokens}"));
                let effort_note = cli
                    .effort
                    .map_or_else(String::new, |e| format!(" · effort {e}"));
                let fallback_note = cli
                    .fallback
                    .as_deref()
                    .map_or_else(String::new, |fb| format!(" · fallback {fb}"));
                format!(
                    "{alias}: {} · model {} · upstream {} · {auth_note}{limit_note}{effort_note}{fallback_note}",
                    cli.kind.as_str(),
                    cli.model,
                    upstream_host(&cli.endpoint),
                )
            }
            Provider::Api(api) => {
                let kind = match api.kind {
                    ApiKind::Anthropic => "anthropic-api",
                    ApiKind::OpenAi => "openai-api",
                };
                let key = match (&api.secret, &api.auth) {
                    (Some(name), _) => format!("secret {name}"),
                    (None, Some(auth)) => {
                        format!("auth {} {}", auth.kind.config_str(), auth.credential)
                    }
                    (None, None) => "no service key (loopback only)".to_owned(),
                };
                let limit =
                    api.max_tokens
                        .map_or_else(String::new, |tokens| match api.limit_field {
                            LimitField::MaxTokens => format!(" · max tokens {tokens}"),
                            other => format!(" · max tokens {tokens} ({})", other.name()),
                        });
                let effort_note = api
                    .effort
                    .map_or_else(String::new, |e| format!(" · effort {e}"));
                let fallback_note = api
                    .fallback
                    .as_deref()
                    .map_or_else(String::new, |fb| format!(" · fallback {fb}"));
                format!(
                    "{alias}: {kind} · model {} · upstream {} · {key}{limit}{effort_note}{fallback_note}",
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
            // Destructure every field explicitly so new fields force an authority review; `Claude` stays tag `1`.
            Provider::Cli(CliProvider {
                kind,
                model,
                endpoint,
                secret,
                auth,
                max_tokens: _,
                effort: _,
                fallback,
            }) => {
                hash.update([match kind {
                    CliKind::Claude => 1,
                    CliKind::Codex => 4,
                    CliKind::Agy => 5,
                }]);
                field(hash, model.as_bytes());
                field(hash, endpoint.as_bytes());
                if let Some(secret) = secret {
                    field(hash, secret.as_bytes());
                } else if let Some(auth) = auth {
                    // Empty secret slot + domain tag keeps all existing secret-based fingerprints byte-identical.
                    field(hash, b"");
                    hash.update(b"auth\0");
                    field(hash, auth.kind.config_str().as_bytes());
                    field(hash, auth.credential.as_bytes());
                } else {
                    field(hash, b"");
                }
                if let Some(target) = fallback {
                    hash.update(b"fallback\0");
                    field(hash, target.as_bytes());
                }
            }
            // Output limits and effort are operational tuning, not authority. Destructure every field explicitly so new fields force a review.
            Provider::Api(ApiProvider {
                kind,
                model,
                endpoint,
                secret,
                auth,
                max_tokens: _,
                limit_field: _,
                effort: _,
                fallback,
            }) => {
                hash.update([match kind {
                    ApiKind::Anthropic => 2,
                    ApiKind::OpenAi => 3,
                }]);
                field(hash, model.as_bytes());
                field(hash, endpoint.as_bytes());
                match (secret, auth) {
                    (Some(name), None) => {
                        hash.update([1]);
                        field(hash, name.as_bytes());
                    }
                    (None, None) => hash.update([0]),
                    (_, Some(auth)) => {
                        hash.update([2]);
                        field(hash, auth.kind.config_str().as_bytes());
                        field(hash, auth.credential.as_bytes());
                    }
                }
                if let Some(target) = fallback {
                    hash.update(b"fallback\0");
                    field(hash, target.as_bytes());
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
