//! Production HTTPS transport (`bot_broker::model::Upstream`) for model proxy upstream calls.
// Disables retries, redirects, and idle connection pooling to avoid hidden duplicate sends;
// marks all request headers sensitive so service keys never appear in logs.
use bot_broker::model::{ChunkSource, Upstream, UpstreamRequest, UpstreamResponse, UpstreamStream};
use bot_core::port::{Error, Reply};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
// Exceeds the proxy's 8 MiB response cap so the proxy enforces the user-facing limit.
const MAX_BODY: usize = 16 << 20;

pub struct HttpsUpstream {
    client: Client,
    scheme: &'static str,
    max_body: usize,
}

impl HttpsUpstream {
    pub fn new() -> Result<Self, String> {
        Self::build("https", MAX_BODY)
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn over_http(max_body: usize) -> Result<Self, String> {
        Self::build("http", max_body)
    }

    fn build(scheme: &'static str, max_body: usize) -> Result<Self, String> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            // Disable idle pool reuse to prevent silent client retries on half-closed sockets.
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(Policy::none())
            .build()
            .map_err(|e| {
                format!("failed to build upstream HTTPS client (check CA certificates): {e}")
            })?;
        Ok(Self {
            client,
            scheme,
            max_body,
        })
    }

    fn url(&self, request: &UpstreamRequest) -> String {
        let host = &request.endpoint.host;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        format!(
            "{}://{host}:{}{}",
            self.scheme, request.endpoint.port, request.path
        )
    }

    fn request(&self, request: UpstreamRequest) -> Result<reqwest::RequestBuilder, Error> {
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Error::Invalid)?;
            let mut value = HeaderValue::from_str(value).map_err(|_| Error::Invalid)?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
        Ok(self
            .client
            .post(self.url(&request))
            .headers(headers)
            .body(request.body))
    }
}

fn unavailable(error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() || error.is_decode() {
        "body"
    } else {
        "request"
    };
    tracing::warn!(kind, "model upstream request failed");
    Error::Unavailable
}

impl Upstream for HttpsUpstream {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse> {
        Box::pin(async move {
            let mut response = self
                .request(request)?
                .send()
                .await
                .map_err(|e| unavailable(&e))?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|e| unavailable(&e))? {
                if body.len() + chunk.len() > self.max_body {
                    tracing::warn!("model upstream response exceeded size limit");
                    return Err(Error::Unavailable);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(UpstreamResponse { status, body })
        })
    }

    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream> {
        Box::pin(async move {
            let response = self
                .request(request)?
                .send()
                .await
                .map_err(|e| unavailable(&e))?;
            Ok(UpstreamStream {
                status: response.status().as_u16(),
                body: Box::new(Chunks(response)),
            })
        })
    }
}

struct Chunks(reqwest::Response);

impl ChunkSource for Chunks {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            self.0
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|e| unavailable(&e))
        })
    }
}

#[cfg(test)]
mod tests;
