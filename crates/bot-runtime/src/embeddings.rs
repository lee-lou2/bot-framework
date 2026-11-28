//! OpenAI-compatible `/embeddings` HTTP client (`Embedder`) with activation fencing and leak scanning.
use crate::model::api::{ApiClient, ApiEndpoint, scan::Needles};
use bot_core::{
    active::ActiveCheck,
    id::{ProfileId, SecretName},
    memory::{Embedder, MAX_EMBEDDING_DIMENSIONS, MemoryEmbedding},
    port::{Error, Reply, SecretStore},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

const MIN_KEY_BYTES: usize = 8;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct EmbeddingSettings {
    pub endpoint: ApiEndpoint,
    pub model: String,
    /// Profile secret name; `None` is permitted only for loopback IP endpoints.
    pub secret: Option<SecretName>,
    pub dimensions: u32,
}

pub struct EmbeddingClient {
    settings: EmbeddingSettings,
    client: ApiClient,
    secrets: Arc<dyn SecretStore>,
    gate: Arc<dyn ActiveCheck>,
}

impl EmbeddingClient {
    pub fn new(
        settings: EmbeddingSettings,
        secrets: Arc<dyn SecretStore>,
        gate: Arc<dyn ActiveCheck>,
    ) -> Result<Self, String> {
        if settings.secret.is_none() && !settings.endpoint.is_loopback() {
            return Err(
                "embeddings.secret is required unless endpoint is a loopback IP address".to_owned(),
            );
        }
        if settings.model.is_empty() || settings.model.len() > 128 {
            return Err("embeddings.model must be 1..=128 bytes".to_owned());
        }
        if !(1..=MAX_EMBEDDING_DIMENSIONS).contains(&settings.dimensions) {
            return Err(format!(
                "embeddings.dimensions must be in 1..={MAX_EMBEDDING_DIMENSIONS}"
            ));
        }
        let client = ApiClient::new(settings.endpoint.is_loopback())?;
        Ok(Self {
            settings,
            client,
            secrets,
            gate,
        })
    }

    async fn resolve_key(&self, profile: &ProfileId) -> Result<Option<String>, Error> {
        let Some(secret_name) = &self.settings.secret else {
            return Ok(None);
        };
        let handle = self.secrets.for_profile(profile.clone()).await?;
        let raw = handle.resolve(secret_name.clone()).await?;
        let key = String::from_utf8(raw.0).map_err(|_| Error::Unavailable)?;
        let key = key.trim().to_owned();
        if key.len() < MIN_KEY_BYTES || key.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::Unavailable);
        }
        Ok(Some(key))
    }

    async fn embed_inner(
        &self,
        profile: &ProfileId,
        text: &str,
    ) -> Result<Option<MemoryEmbedding>, Error> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        // Double active-revision guard: before reading the secret and immediately before sending.
        self.gate.verify().await?;
        let key = self.resolve_key(profile).await?;
        self.gate.verify().await?;

        let mut headers = vec![(
            "content-type".to_owned(),
            "application/json".to_owned(),
            false,
        )];
        let needles = key.as_deref().map(Needles::new);
        if let Some(token) = &key {
            headers.push(("authorization".to_owned(), format!("Bearer {token}"), true));
        }
        let body = json!({
            "model": self.settings.model,
            "input": trimmed,
            "dimensions": self.settings.dimensions,
        })
        .to_string()
        .into_bytes();
        let url = self.settings.endpoint.url("/embeddings");
        let answer = self
            .client
            .post(&url, headers, body, REQUEST_TIMEOUT)
            .await
            .map_err(|_| Error::Unavailable)?;
        if !(200..300).contains(&answer.status) {
            tracing::warn!(status = answer.status, "embedding request rejected");
            return Err(Error::Unavailable);
        }
        let raw = String::from_utf8(answer.body).map_err(|_| Error::Unavailable)?;
        if needles.as_ref().is_some_and(|scan| scan.in_body(&raw)) {
            tracing::warn!(
                code = "secret_in_response",
                "discarded embedding response containing key"
            );
            return Err(Error::Unavailable);
        }
        let values = parse_embedding(&raw, self.settings.dimensions as usize)?;
        MemoryEmbedding::normalize(&self.settings.model, values)
            .map(Some)
            .ok_or(Error::Unavailable)
    }
}

impl Embedder for EmbeddingClient {
    fn model(&self) -> &str {
        &self.settings.model
    }

    fn dimensions(&self) -> u32 {
        self.settings.dimensions
    }

    fn embed<'a>(
        &'a self,
        profile: &'a ProfileId,
        text: &'a str,
    ) -> Reply<'a, Option<MemoryEmbedding>> {
        Box::pin(self.embed_inner(profile, text))
    }
}

fn parse_embedding(raw: &str, expected_dim: usize) -> Result<Vec<f32>, Error> {
    let parsed: Value = serde_json::from_str(raw).map_err(|_| Error::Unavailable)?;
    let arr = parsed
        .get("data")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("embedding"))
        .and_then(Value::as_array)
        .ok_or(Error::Unavailable)?;
    if arr.len() != expected_dim {
        return Err(Error::Unavailable);
    }
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let v = entry.as_f64().ok_or(Error::Unavailable)? as f32;
        if !v.is_finite() {
            return Err(Error::Unavailable);
        }
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{
        memory::DEFAULT_EMBEDDING_DIMENSIONS,
        port::{ProfileSecrets, SecretValue},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    struct StaticSecrets(String);
    impl ProfileSecrets for StaticSecrets {
        fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
            let v = self.0.as_bytes().to_vec();
            Box::pin(async move { Ok(SecretValue(v)) })
        }
    }
    impl SecretStore for StaticSecrets {
        fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
            let s = self.0.clone();
            Box::pin(async move { Ok(Box::new(StaticSecrets(s)) as Box<dyn ProfileSecrets>) })
        }
    }

    struct Gate(bool);
    impl ActiveCheck for Gate {
        fn verify(&self) -> Reply<'_, ()> {
            let ok = self.0;
            Box::pin(async move { if ok { Ok(()) } else { Err(Error::Stale) } })
        }
    }

    async fn mock_server(status: u16, body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (format!("http://127.0.0.1:{}/v1", addr.port()), handle)
    }

    #[tokio::test]
    async fn posts_dimensions_normalizes_and_rejects_secret_leaks_and_stale_gates() {
        let vec_json: Vec<f32> = (0..DEFAULT_EMBEDDING_DIMENSIONS)
            .map(|i| {
                if i == 0 {
                    3.0
                } else if i == 1 {
                    4.0
                } else {
                    0.0
                }
            })
            .collect();
        let ok_body = json!({"data": [{"embedding": vec_json}]}).to_string();
        let (base, server) = mock_server(200, ok_body).await;
        let client = EmbeddingClient::new(
            EmbeddingSettings {
                endpoint: ApiEndpoint::parse(&base).unwrap(),
                model: "gemini-embedding-2".into(),
                secret: Some(SecretName::parse("gemini-api-key").unwrap()),
                dimensions: DEFAULT_EMBEDDING_DIMENSIONS,
            },
            Arc::new(StaticSecrets("secret-token-1234".into())),
            Arc::new(Gate(true)),
        )
        .unwrap();
        let emb = client
            .embed(&ProfileId("personal".into()), "hello")
            .await
            .unwrap()
            .unwrap();
        let raw_req = server.await.unwrap();
        assert!(raw_req.contains("POST /v1/embeddings HTTP/1.1"));
        assert!(raw_req.contains("authorization: Bearer secret-token-1234"));
        assert!(raw_req.contains("\"dimensions\":3072"));
        assert_eq!(emb.dim(), 3072);
        assert!((emb.values[0] - 0.6).abs() < 1e-6);
        assert!((emb.values[1] - 0.8).abs() < 1e-6);

        // Stale gate refuses before hitting upstream.
        let stale = EmbeddingClient::new(
            EmbeddingSettings {
                endpoint: ApiEndpoint::parse(&base).unwrap(),
                model: "gemini-embedding-2".into(),
                secret: None,
                dimensions: 2,
            },
            Arc::new(StaticSecrets("secret-token-1234".into())),
            Arc::new(Gate(false)),
        )
        .unwrap();
        assert!(matches!(
            stale.embed(&ProfileId("personal".into()), "hello").await,
            Err(Error::Stale)
        ));

        // Response containing the request key is discarded.
        let leak_body =
            json!({"data": [{"embedding": [1.0, 0.0]}], "echo": "secret-token-1234"}).to_string();
        let (base2, _srv2) = mock_server(200, leak_body).await;
        let leak = EmbeddingClient::new(
            EmbeddingSettings {
                endpoint: ApiEndpoint::parse(&base2).unwrap(),
                model: "gemini-embedding-2".into(),
                secret: Some(SecretName::parse("gemini-api-key").unwrap()),
                dimensions: 2,
            },
            Arc::new(StaticSecrets("secret-token-1234".into())),
            Arc::new(Gate(true)),
        )
        .unwrap();
        assert!(matches!(
            leak.embed(&ProfileId("personal".into()), "hello").await,
            Err(Error::Unavailable)
        ));
    }
}
