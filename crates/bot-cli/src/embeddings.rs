//! Host `embeddings` section parsing, authority-fingerprint binding, and `EmbeddingClient` assembly.
use crate::serve::watched::Watched;
use bot_core::{
    active::ActiveCheck,
    id::SecretName,
    memory::{DEFAULT_EMBEDDING_DIMENSIONS, Embedder, MAX_EMBEDDING_DIMENSIONS},
};
use bot_runtime::{
    embeddings::{EmbeddingClient, EmbeddingSettings},
    model::ApiEndpoint,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingConfig {
    pub endpoint: String,
    pub model: String,
    /// Profile secret name; may be omitted only for loopback endpoints.
    pub secret: Option<String>,
    /// Requested output dimensionality (defaults to 3072, maximum for `gemini-embedding-2`).
    pub dimensions: u32,
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

pub fn parse(value: &Value) -> Result<EmbeddingConfig, String> {
    let Value::Object(fields) = value else {
        return Err("embeddings: must be an object".to_owned());
    };
    only(
        fields,
        &["endpoint", "model", "secret", "dimensions"],
        "embeddings",
    )?;
    let endpoint_text = text(fields, "endpoint", "embeddings")?;
    let endpoint = ApiEndpoint::parse(endpoint_text)
        .map_err(|reason| format!("embeddings.endpoint: {reason}"))?;
    let model = text(fields, "model", "embeddings")?;
    if model.len() > 128 {
        return Err("embeddings.model: must be at most 128 bytes".to_owned());
    }
    let secret = match fields.get("secret") {
        None => None,
        Some(_) => {
            let name = text(fields, "secret", "embeddings")?;
            if SecretName::parse(name).is_none() {
                return Err("embeddings.secret: invalid secret name".to_owned());
            }
            Some(name.to_owned())
        }
    };
    if secret.is_none() && !endpoint.is_loopback() {
        return Err(
            "embeddings.secret: service key secret name is required (omitting secret is only allowed for loopback IP addresses)"
                .to_owned(),
        );
    }
    let dimensions = match fields.get("dimensions") {
        None => DEFAULT_EMBEDDING_DIMENSIONS,
        Some(raw) => raw
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=MAX_EMBEDDING_DIMENSIONS).contains(n))
            .ok_or_else(|| {
                format!(
                    "embeddings.dimensions: must be an integer between 1 and {MAX_EMBEDDING_DIMENSIONS}"
                )
            })?,
    };
    Ok(EmbeddingConfig {
        endpoint: endpoint_text.to_owned(),
        model: model.to_owned(),
        secret,
        dimensions,
    })
}

/// Binds configured embeddings into the activation fingerprint while leaving omitted deployments byte-identical.
pub fn bind(base: [u8; 32], config: Option<&EmbeddingConfig>) -> [u8; 32] {
    let Some(cfg) = config else {
        return base;
    };
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-embeddings-v1\0");
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
    hash.update(cfg.dimensions.to_be_bytes());
    hash.finalize().into()
}

/// Builds a live `Embedder` adapter when `host.embeddings` is configured.
pub fn build(
    config: Option<&EmbeddingConfig>,
    secrets: Arc<Watched>,
    gate: Arc<dyn ActiveCheck>,
) -> Result<Option<Arc<dyn Embedder>>, String> {
    let Some(cfg) = config else {
        return Ok(None);
    };
    let endpoint = ApiEndpoint::parse(&cfg.endpoint)
        .map_err(|reason| format!("embeddings.endpoint: {reason}"))?;
    let secret = match cfg.secret.as_deref() {
        Some(name) => Some(
            SecretName::parse(name)
                .ok_or_else(|| "embeddings.secret: invalid secret name".to_owned())?,
        ),
        None => None,
    };
    let client = EmbeddingClient::new(
        EmbeddingSettings {
            endpoint,
            model: cfg.model.clone(),
            secret,
            dimensions: cfg.dimensions,
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
    fn defaults_to_max_dimension_3072_and_validates_endpoints_and_secrets() {
        let cfg = parse(&json!({
            "endpoint": "https://generativelanguage.googleapis.com/v1beta/openai",
            "model": "gemini-embedding-2",
            "secret": "gemini-api-key"
        }))
        .unwrap();
        assert_eq!(cfg.dimensions, 3072);
        assert_eq!(cfg.secret.as_deref(), Some("gemini-api-key"));

        let local = parse(&json!({
            "endpoint": "http://127.0.0.1:11434/v1",
            "model": "embeddinggemma-2:latest",
            "dimensions": 768
        }))
        .unwrap();
        assert_eq!(local.dimensions, 768);
        assert!(local.secret.is_none());

        for bad in [
            json!("bad"),
            json!({"endpoint": "https://api.example/v1", "model": "m"}),
            json!({"endpoint": "http://api.example/v1", "model": "m", "secret": "k"}),
            json!({"endpoint": "https://api.example/v1", "model": "m", "secret": "k", "dimensions": 0}),
            json!({"endpoint": "https://api.example/v1", "model": "m", "secret": "k", "dimensions": 4097}),
            json!({"endpoint": "https://api.example/v1", "model": "m", "secret": "k", "extra": 1}),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fingerprint_stays_identical_when_omitted_and_changes_on_any_setting() {
        let base = [9u8; 32];
        assert_eq!(bind(base, None), base);
        let a = parse(&json!({
            "endpoint": "https://generativelanguage.googleapis.com/v1beta/openai",
            "model": "gemini-embedding-2",
            "secret": "gemini-api-key"
        }))
        .unwrap();
        let b = EmbeddingConfig {
            dimensions: 768,
            ..a.clone()
        };
        assert_ne!(bind(base, Some(&a)), base);
        assert_ne!(bind(base, Some(&a)), bind(base, Some(&b)));
        assert_eq!(bind(base, Some(&a)), bind(base, Some(&a)));
    }
}
