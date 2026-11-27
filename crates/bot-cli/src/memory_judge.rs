//! Host `memory_judge` section parsing, authority-fingerprint binding, and `MemoryJudgeClient` assembly.
use crate::serve::watched::Watched;
use bot_core::{active::ActiveCheck, id::SecretName, memory::MemoryJudge};
use bot_runtime::{
    memory_judge::{MemoryJudgeClient, MemoryJudgeSettings},
    model::ApiEndpoint,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryJudgeConfig {
    pub endpoint: String,
    pub model: String,
    /// Profile secret name; may be omitted only for loopback endpoints.
    pub secret: Option<String>,
}

fn only(map: &Map<String, Value>, allowed: &[&str], path: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(format!("{path}: unknown key: {unknown}")),
        None => Ok(()),
    }
}

fn text<'a>(map: &'a Map<String, Value>, key: &str, path: &str) -> Result<&'a str, String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty() && !v.contains('\0'))
        .ok_or_else(|| format!("{path}.{key}: must be a non-empty string"))
}

pub fn parse(value: &Value) -> Result<MemoryJudgeConfig, String> {
    let Value::Object(fields) = value else {
        return Err("memory_judge: must be an object".to_owned());
    };
    only(fields, &["endpoint", "model", "secret"], "memory_judge")?;
    let endpoint_text = text(fields, "endpoint", "memory_judge")?;
    let endpoint = ApiEndpoint::parse(endpoint_text)
        .map_err(|reason| format!("memory_judge.endpoint: {reason}"))?;
    let model = text(fields, "model", "memory_judge")?;
    if model.len() > 128 {
        return Err("memory_judge.model: must be at most 128 bytes".to_owned());
    }
    let secret = match fields.get("secret") {
        None => None,
        Some(_) => {
            let name = text(fields, "secret", "memory_judge")?;
            if SecretName::parse(name).is_none() {
                return Err("memory_judge.secret: invalid secret name".to_owned());
            }
            Some(name.to_owned())
        }
    };
    if secret.is_none() && !endpoint.is_loopback() {
        return Err(
            "memory_judge.secret: service key secret name is required (omitting secret is only allowed for loopback IP addresses)"
                .to_owned(),
        );
    }
    Ok(MemoryJudgeConfig {
        endpoint: endpoint_text.to_owned(),
        model: model.to_owned(),
        secret,
    })
}

/// Binds configured memory_judge into the activation fingerprint while leaving omitted deployments byte-identical.
pub fn bind(base: [u8; 32], config: Option<&MemoryJudgeConfig>) -> [u8; 32] {
    let Some(cfg) = config else {
        return base;
    };
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-memory-judge-v1\0");
    hash.update(base);
    for bytes in [cfg.endpoint.as_bytes(), cfg.model.as_bytes()] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    match &cfg.secret {
        Some(secret) => {
            hash.update([1]);
            hash.update((secret.len() as u64).to_be_bytes());
            hash.update(secret.as_bytes());
        }
        None => hash.update([0]),
    }
    hash.finalize().into()
}

/// Builds a live `MemoryJudge` adapter when `host.memory_judge` is configured.
pub fn build(
    config: Option<&MemoryJudgeConfig>,
    secrets: Arc<Watched>,
    gate: Arc<dyn ActiveCheck>,
) -> Result<Option<Arc<dyn MemoryJudge>>, String> {
    let Some(cfg) = config else {
        return Ok(None);
    };
    let endpoint = ApiEndpoint::parse(&cfg.endpoint)
        .map_err(|reason| format!("memory_judge.endpoint: {reason}"))?;
    let secret = match cfg.secret.as_deref() {
        Some(name) => Some(
            SecretName::parse(name)
                .ok_or_else(|| "memory_judge.secret: invalid secret name".to_owned())?,
        ),
        None => None,
    };
    let client = MemoryJudgeClient::new(
        MemoryJudgeSettings {
            endpoint,
            model: cfg.model.clone(),
            secret,
        },
        secrets,
        gate,
    )?;
    Ok(Some(Arc::new(client)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_liquid_typesafe_and_loopback_configs_and_rejects_bad_inputs() {
        let liquid = parse(&json!({
            "endpoint": "https://api.liquid.ai/decisions/v1/systemone",
            "model": "d1.2-ultra",
            "secret": "liquid-api-key"
        }))
        .unwrap();
        assert_eq!(liquid.model, "d1.2-ultra");
        assert_eq!(liquid.secret.as_deref(), Some("liquid-api-key"));

        let local = parse(&json!({
            "endpoint": "http://127.0.0.1:9000/decisions",
            "model": "d1.2"
        }))
        .unwrap();
        assert!(local.secret.is_none());

        for bad in [
            json!("bad"),
            json!({"endpoint": "https://api.liquid.ai/decisions", "model": "d1.2-ultra"}),
            json!({"endpoint": "http://api.liquid.ai/decisions", "model": "d1.2-ultra", "secret": "k"}),
            json!({"endpoint": "https://api.liquid.ai/decisions", "model": "d1.2-ultra", "secret": "k", "extra": 1}),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fingerprint_stays_identical_when_omitted_and_changes_on_any_setting() {
        let base = [7u8; 32];
        assert_eq!(bind(base, None), base);
        let a = parse(&json!({
            "endpoint": "https://api.liquid.ai/decisions/v1/systemone",
            "model": "d1.2-ultra",
            "secret": "liquid-api-key"
        }))
        .unwrap();
        let b = MemoryJudgeConfig {
            model: "d1.2".into(),
            ..a.clone()
        };
        assert_ne!(bind(base, Some(&a)), base);
        assert_ne!(bind(base, Some(&a)), bind(base, Some(&b)));
        assert_eq!(bind(base, Some(&a)), bind(base, Some(&a)));
        // Fixed-input golden pins to catch accidental activation fingerprint drift.
        assert_eq!(
            bot_core::random::hex(&bind(base, Some(&a))),
            "87cdcf5a97d5839e1b7ed4ca5b5c07589613d33e2b5fdbf673bc78d58f0aace3"
        );
        assert_eq!(
            bot_core::random::hex(&bind(base, Some(&b))),
            "09dfe777311e22f6dc72e0ed78ddca8743f959ea8a9565433daab4619eb27569"
        );
    }
}
