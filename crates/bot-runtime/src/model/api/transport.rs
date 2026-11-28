//! Upstream HTTP POST transport with redirects, retries, and connection pooling disabled.

use crate::model::ModelError;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(30);
pub(crate) const MAX_BODY: usize = 8 << 20;
/// Cap for reading non-2xx error bodies used only for fixed-code classification.
pub(crate) const ERROR_BODY_MAX: usize = 16 << 10;

/// Request header tuple: `(lowercase_name, value, is_sensitive)`.
pub(crate) type Header = (String, String, bool);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Fault {
    Connect,
    Timeout,
    Body,
    TooLarge,
    Request,
}

impl Fault {
    pub(crate) fn into_error(self) -> ModelError {
        match self {
            Fault::Connect | Fault::Timeout | Fault::Body => ModelError::Failed {
                code: "network",
                transient: true,
            },
            Fault::TooLarge => ModelError::Failed {
                code: "invalid_response",
                transient: false,
            },
            Fault::Request => ModelError::Failed {
                code: "prepare_failed",
                transient: false,
            },
        }
    }
}

pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ApiClient {
    client: Client,
}

impl ApiClient {
    pub fn new(loopback: bool) -> Result<Self, String> {
        let mut builder = Client::builder()
            // Some upstream WAFs reject requests without a User-Agent header.
            .user_agent("bot-framework")
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(KEEPALIVE)
            // Disable idle pooling so the client never silently retries on a stale socket.
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .redirect(Policy::none());
        if loopback {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| {
            "failed to build upstream HTTPS client (check system CA certificates)".to_owned()
        })?;
        Ok(Self { client })
    }

    pub(crate) async fn post(
        &self,
        url: &str,
        headers: Vec<Header>,
        body: Vec<u8>,
        idle: Duration,
    ) -> Result<Answer, Fault> {
        let mut map = HeaderMap::new();
        for (name, value, secret) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| Fault::Request)?;
            let mut value = HeaderValue::from_str(&value).map_err(|_| Fault::Request)?;
            value.set_sensitive(secret);
            map.append(name, value);
        }
        let mut response = self
            .client
            .post(url)
            .headers(map)
            .body(body)
            .send()
            .await
            .map_err(|e| fault(&e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = read_prefix(&mut response, idle, ERROR_BODY_MAX).await;
            return Ok(Answer { status, body });
        }
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(idle, response.chunk()).await {
                Err(_) => return Err(Fault::Timeout),
                Ok(Err(e)) => return Err(fault(&e)),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => {
                    if out.len() + chunk.len() > MAX_BODY {
                        return Err(Fault::TooLarge);
                    }
                    out.extend_from_slice(&chunk);
                }
            }
        }
        Ok(Answer { status, body: out })
    }
}

async fn read_prefix(response: &mut reqwest::Response, idle: Duration, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < max {
        match tokio::time::timeout(idle, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let room = max - out.len();
                out.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            _ => break,
        }
    }
    out
}

fn fault(error: &reqwest::Error) -> Fault {
    let fault = if error.is_builder() {
        Fault::Request
    } else if error.is_timeout() {
        Fault::Timeout
    } else if error.is_body() || error.is_decode() {
        Fault::Body
    } else {
        Fault::Connect
    };
    tracing::warn!(kind = ?fault, "upstream model request failed");
    fault
}
