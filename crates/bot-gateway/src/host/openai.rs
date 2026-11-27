//! OpenAI-compatible HTTP endpoint configuration (`kind: openai`) and validation.
//!
//! Shares `gateway.http.endpoints.<alias>` with JWT webhook endpoints (`kind` omitted or `webhook`), rejecting mixed
//! fields before startup. Because OpenAI routes live under root `/v1`, alias `v1` is reserved whenever any OpenAI
//! endpoint is configured.
use super::{HostError, HttpEndpoint, HttpHost, bad, secret};
use crate::identity::{self, is_alias};
use serde::{Deserialize, Deserializer, de::Error as _};
use std::{collections::BTreeMap, time::Duration};

pub const DEFAULT_WAIT_MS: u64 = 120_000;
pub const MAX_WAIT_MS: u64 = 300_000;
pub const DEFAULT_MAX_ACTIVE_RUNS: u32 = 32;
pub const MAX_ACTIVE_RUNS: u32 = 256;
pub const MAX_KEY_SLOTS: usize = 16;
/// Default request body limit when `gateway.limits.body_bytes` is omitted (webhooks default to 64 KiB).
pub const DEFAULT_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiEndpoint {
    /// Slot name → secret name + subject (multiple slots may share a subject for key rotation).
    pub keys: BTreeMap<String, KeySlot>,
    pub wait_ms: Option<u64>,
    pub max_active_runs: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySlot {
    /// Secret name resolved under `bot/<profile>/<name>`; never a raw secret value.
    pub secret: String,
    /// Subject component of `http:<alias>/<subject>`.
    pub subject: String,
}

impl OpenAiEndpoint {
    pub fn wait(&self) -> Duration {
        Duration::from_millis(self.wait_ms.unwrap_or(DEFAULT_WAIT_MS))
    }

    pub fn max_active(&self) -> u32 {
        self.max_active_runs.unwrap_or(DEFAULT_MAX_ACTIVE_RUNS)
    }
}

const WEBHOOK_ONLY: [&str; 5] = [
    "issuer",
    "audience",
    "key",
    "sync_wait_ms",
    "max_token_age_secs",
];
const OPENAI_ONLY: [&str; 3] = ["keys", "wait_ms", "max_active_runs"];

enum Parsed {
    Webhook(HttpEndpoint),
    OpenAi(OpenAiEndpoint),
}

fn endpoint_of(alias: &str, value: serde_json::Value) -> Result<Parsed, String> {
    let path = format!("gateway.http.endpoints.{alias}");
    let serde_json::Value::Object(mut map) = value else {
        return Err(format!("{path}: must be an object"));
    };
    let openai = match map.remove("kind") {
        None => false,
        Some(serde_json::Value::String(kind)) if kind == "webhook" => false,
        Some(serde_json::Value::String(kind)) if kind == "openai" => true,
        Some(_) => return Err(format!("{path}.kind: must be webhook or openai")),
    };
    let (kind, foreign) = if openai {
        ("openai", WEBHOOK_ONLY.as_slice())
    } else {
        ("webhook", OPENAI_ONLY.as_slice())
    };
    if let Some(field) = foreign.iter().find(|field| map.contains_key(**field)) {
        return Err(format!(
            "{path}.{field}: not allowed on a kind={kind} endpoint (do not mix webhook and openai fields)"
        ));
    }
    let value = serde_json::Value::Object(map);
    if openai {
        serde_json::from_value(value)
            .map(Parsed::OpenAi)
            .map_err(|e| format!("{path}: {e}"))
    } else {
        serde_json::from_value(value)
            .map(Parsed::Webhook)
            .map_err(|e| format!("{path}: {e}"))
    }
}

impl<'de> Deserialize<'de> for HttpHost {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            listen: Option<String>,
            #[serde(default)]
            remote: bool,
            #[serde(default)]
            endpoints: BTreeMap<String, serde_json::Value>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let mut host = HttpHost {
            listen: raw.listen,
            remote: raw.remote,
            ..HttpHost::default()
        };
        for (alias, value) in raw.endpoints {
            match endpoint_of(&alias, value).map_err(D::Error::custom)? {
                Parsed::Webhook(endpoint) => {
                    host.endpoints.insert(alias, endpoint);
                }
                Parsed::OpenAi(endpoint) => {
                    host.openai.insert(alias, endpoint);
                }
            }
        }
        Ok(host)
    }
}

/// Validates OpenAI endpoints and webhook collisions without echoing config values.
pub(super) fn validate(http: &HttpHost) -> Result<(), HostError> {
    if http.openai.is_empty() {
        return Ok(());
    }
    if http.endpoints.contains_key("v1") {
        return Err(bad(
            "http.endpoints.v1",
            "webhook alias v1 cannot coexist with an OpenAI endpoint because OpenAI uses the root /v1 path — rename the webhook alias",
        ));
    }
    for (alias, endpoint) in &http.openai {
        let path = format!("http.endpoints.{alias}");
        if !is_alias(alias) {
            return Err(bad(
                &path,
                "alias must be lowercase kebab-case of at most 63 characters",
            ));
        }
        if alias == "v1" || alias.starts_with('_') {
            return Err(bad(&path, "reserved name (root /v1 path)"));
        }
        if endpoint.keys.is_empty() || endpoint.keys.len() > MAX_KEY_SLOTS {
            return Err(bad(
                &format!("{path}.keys"),
                &format!("must have 1..={MAX_KEY_SLOTS} key slots"),
            ));
        }
        let mut names = BTreeMap::new();
        for (slot, key) in &endpoint.keys {
            let slot_path = format!("{path}.keys.{slot}");
            if !is_alias(slot) {
                return Err(bad(
                    &slot_path,
                    "slot name must be lowercase kebab-case of at most 63 characters",
                ));
            }
            secret(&format!("{slot_path}.secret"), &key.secret)?;
            if identity::http(alias, &key.subject).is_none() {
                return Err(bad(
                    &format!("{slot_path}.subject"),
                    "subject must be at most 128 characters using only ASCII alphanumerics and . _ - @ +",
                ));
            }
            // Reject duplicate secret names across slots so one key cannot authenticate ambiguously.
            if let Some(first) = names.insert(&key.secret, slot) {
                return Err(bad(
                    &format!("{slot_path}.secret"),
                    &format!(
                        "slot {first} already uses the same secret name (one key would match two slots)"
                    ),
                ));
            }
        }
        if endpoint
            .wait_ms
            .is_some_and(|ms| ms == 0 || ms > MAX_WAIT_MS)
        {
            return Err(bad(&path, &format!("wait_ms must be in 1..={MAX_WAIT_MS}")));
        }
        if endpoint
            .max_active_runs
            .is_some_and(|n| n == 0 || n > MAX_ACTIVE_RUNS)
        {
            return Err(bad(
                &path,
                &format!("max_active_runs must be in 1..={MAX_ACTIVE_RUNS}"),
            ));
        }
    }
    Ok(())
}

/// API key format shared by authentication and startup checks: 32..=256 ASCII graphic bytes.
pub fn key_shaped(bytes: &[u8]) -> bool {
    (MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&bytes.len()) && bytes.iter().all(u8::is_ascii_graphic)
}

const MIN_KEY_BYTES: usize = 32;
const MAX_KEY_BYTES: usize = 256;

#[cfg(test)]
mod tests;
