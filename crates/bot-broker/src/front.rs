//! Sandbox-facing HTTP entry point for model relay (`/v1/messages`, `/v1/responses`, `/v1beta/models/...`) and MCP (`/mcp`).
//!
//! Both routes share one per-attempt session token (`x-api-key`, `x-goog-api-key`, or
//! `Authorization: Bearer`) stored only as a SHA-256 hash. Every request checks live claim
//! ownership against the ledger; model alias, protocol, agent, principal, and logical call ID are
//! host-bound. Connections close per request and emit fixed-vocabulary error bodies only.

mod agy_calls;
pub mod cli;
mod delegate;
mod http;
mod mcp;
pub mod observe;
mod owner;
pub mod park;
mod refuse;
mod relay;
mod slots;
#[cfg(test)]
mod tests;

use crate::{
    canon::sha256,
    model::{ModelProxy, Protocol},
    sys::Entropy,
    text::hex,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    ledger::CallLedger,
    port::{Broker, DelegationStore, Error, Reply},
    profile::ProfileCatalog,
};
use http::{Request, read_request, write_response};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct Session {
    pub attempt: Attempt,
    pub agent: AgentId,
    pub alias: String,
    pub protocol: Protocol,
}

struct Live {
    session: Session,
    gone: watch::Sender<bool>,
}

/// Maps hashed session tokens to active attempts and revocation channels.
pub struct SessionTable {
    entropy: Arc<dyn Entropy>,
    map: Mutex<HashMap<String, Live>>,
}

fn token_key(token: &str) -> String {
    hex(&sha256(token))
}

impl SessionTable {
    pub fn new(entropy: Arc<dyn Entropy>) -> Arc<Self> {
        Arc::new(Self {
            entropy,
            map: Mutex::new(HashMap::new()),
        })
    }

    pub fn issue(&self, session: Session) -> Option<String> {
        let mut raw = [0u8; 32];
        if !self.entropy.fill(&mut raw) {
            return None;
        }
        let token = format!("bot-s-{}", hex(&raw));
        let (gone, _) = watch::channel(false);
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token_key(&token), Live { session, gone });
        Some(token)
    }

    pub fn revoke(&self, token: &str) {
        let removed = self
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&token_key(token));
        if let Some(live) = removed {
            let _ = live.gone.send(true);
        }
    }

    /// Revokes only tokens matching the exact `Attempt` (including claim) so an exiting worker
    /// never disconnects a newer claim's session.
    pub fn revoke_attempt(&self, attempt: &Attempt) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, live| {
            let mine = live.session.attempt == *attempt;
            if mine {
                let _ = live.gone.send(true);
            }
            !mine
        });
    }

    fn get(&self, token: &str) -> Option<(Session, watch::Receiver<bool>)> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&token_key(token))
            .map(|live| (live.session.clone(), live.gone.subscribe()))
    }

    pub(super) fn active_attempt_keys(
        &self,
    ) -> std::collections::HashSet<(String, String, u64, Option<u64>)> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|live| {
                let a = &live.session.attempt;
                (
                    a.profile.0.clone(),
                    a.run.0.clone(),
                    a.generation.0,
                    a.claim.map(|c| c.0),
                )
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrontConfig {
    pub head_timeout: Duration,
    pub body_timeout: Duration,
    pub write_timeout: Duration,
    pub max_connections: usize,
    pub max_body: usize,
    /// Interval for re-verifying claim ownership while a model request is open.
    pub revoke_check: Duration,
    /// Grace window to pause output while the ledger is unreachable before closing.
    pub verify_grace: Duration,
    /// Bound for persisting a completed response's observed tool plan.
    pub publish_timeout: Duration,
}

impl Default for FrontConfig {
    fn default() -> Self {
        Self {
            head_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            max_connections: 64,
            max_body: 4 << 20,
            revoke_check: Duration::from_secs(2),
            verify_grace: Duration::from_secs(10),
            publish_timeout: Duration::from_secs(15),
        }
    }
}

/// Maps CLI tool invocations (`toolUseId`, name, canonical args) to deterministic logical `CallId`s
/// so redeliveries and resumes never duplicate execution. Production uses `cli::PlannedCalls`.
pub trait CallBook: Send + Sync {
    fn call_id(
        &self,
        attempt: Attempt,
        tool_use: String,
        name: String,
        args: JsonPayload,
    ) -> Reply<'_, CallId>;

    /// Reads an existing pre-execution refusal marker before invoking the broker so redeliveries
    /// replay the exact refusal word instead of executing.
    fn marked(&self, _: Attempt, _: CallId) -> Reply<'_, Option<bot_core::cli::CliRefusal>> {
        Box::pin(async { Ok(None) })
    }

    /// Confirms whether a refusal at a verified CLI call slot is deterministic pre-execution and
    /// therefore safe to persist as a checkpoint refusal marker.
    fn confirm(
        &self,
        _: Attempt,
        _: CallId,
        _: ToolId,
        _: JsonPayload,
        _: bot_core::cli::CliRefusal,
    ) -> Reply<'_, bool> {
        Box::pin(async { Ok(false) })
    }
}

/// Stateless test/simple `CallBook` (`m-<toolUseId>`); never used for production CLI resumes.
pub struct DirectIds;

impl CallBook for DirectIds {
    fn call_id(
        &self,
        _: Attempt,
        tool_use: String,
        _: String,
        _: JsonPayload,
    ) -> Reply<'_, CallId> {
        Box::pin(async move { Ok(CallId(format!("m-{tool_use}"))) })
    }
}

pub struct FrontParts {
    pub sessions: Arc<SessionTable>,
    pub model: Arc<ModelProxy>,
    pub broker: Arc<dyn Broker>,
    pub catalog: Arc<dyn ProfileCatalog>,
    pub calls: Arc<dyn CallBook>,
    /// Persists completed turn tool lists before releasing the terminal response to the CLI.
    pub turns: Arc<dyn observe::TurnSink>,
    pub ledger: Arc<dyn CallLedger>,
    pub delegation: Arc<dyn DelegationStore>,
    pub park: Arc<dyn park::Park>,
}

struct Shared {
    parts: FrontParts,
    cfg: FrontConfig,
    slots: slots::Slots,
    agy: agy_calls::AgyCalls,
}

pub struct ModelFront {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
    #[cfg(test)]
    shared: Arc<Shared>,
}

pub(crate) fn error_body(kind: &str, message: &str) -> String {
    format!("{{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{message}\"}}}}")
}

pub(crate) fn protocol_error_body(
    protocol: Protocol,
    code: u16,
    kind: &str,
    message: &str,
) -> String {
    match protocol {
        Protocol::Anthropic => error_body(kind, message),
        Protocol::OpenAiResponses => {
            format!("{{\"error\":{{\"type\":\"{kind}\",\"message\":\"{message}\"}}}}")
        }
        Protocol::Gemini => {
            let status = match code {
                400 => "INVALID_ARGUMENT",
                401 => "UNAUTHENTICATED",
                403 => "PERMISSION_DENIED",
                404 => "NOT_FOUND",
                _ => "UNAVAILABLE",
            };
            format!(
                "{{\"error\":{{\"code\":{code},\"message\":\"{message}\",\"status\":\"{status}\"}}}}"
            )
        }
    }
}

/// Parses `/v1beta/models/<model>:streamGenerateContent` (`Some(true)`) or `:generateContent` (`Some(false)`).
pub(super) fn gemini_route(path: &str) -> Option<bool> {
    let rest = path.strip_prefix("/v1beta/models/")?;
    let (model, streaming) = if let Some(m) = rest.strip_suffix(":streamGenerateContent") {
        (m, true)
    } else {
        let m = rest.strip_suffix(":generateContent")?;
        (m, false)
    };
    let ok = !model.is_empty()
        && model.len() <= 128
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ok.then_some(streaming)
}

/// Maps broker errors to HTTP status and error types (`Stale` uses 403 so CLIs do not retry).
fn http_error(error: &Error) -> (u16, &'static str, &'static str) {
    match error {
        Error::Invalid | Error::Conflict => (400, "invalid_request_error", "invalid"),
        Error::Denied => (403, "permission_error", "denied"),
        Error::Stale => (403, "permission_error", "stale"),
        Error::Missing => (404, "not_found_error", "missing"),
        Error::Unavailable | Error::RateLimited => (503, "overloaded_error", "unavailable"),
    }
}

fn bearer(request: &Request) -> Option<&str> {
    request
        .header("x-api-key")
        .or_else(|| request.header("x-goog-api-key"))
        .or_else(|| {
            request
                .header("authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
        })
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

impl ModelFront {
    pub async fn start(bind: SocketAddr, parts: FrontParts, cfg: FrontConfig) -> io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let addr = listener.local_addr()?;
        let slots = Arc::new(Semaphore::new(cfg.max_connections));
        let shared = Arc::new(Shared {
            parts,
            cfg,
            slots: slots::Slots::default(),
            agy: agy_calls::AgyCalls::default(),
        });
        let (stop, signal) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(listener, shared.clone(), slots, signal));
        Ok(Self {
            addr,
            stop,
            accept,
            #[cfg(test)]
            shared,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.accept.await;
    }
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else { continue };
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else { continue };
                let (shared, mut stop) = (Arc::clone(&shared), stop.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        _ = serve(client, &shared) => {}
                        _ = stop.changed() => {}
                    }
                    drop(permit);
                });
            }
        }
    }
}

async fn serve(mut client: TcpStream, shared: &Shared) {
    let cfg = &shared.cfg;
    let request = match read_request(
        &mut client,
        cfg.max_body,
        cfg.head_timeout,
        cfg.body_timeout,
    )
    .await
    {
        Ok(request) => request,
        Err(status) => {
            let body = error_body("invalid_request_error", status.message());
            return write_response(
                &mut client,
                status.code(),
                "application/json",
                body.as_bytes(),
                cfg.write_timeout,
            )
            .await;
        }
    };
    let reply = |code: u16, kind: &str, message: &str| {
        let body = error_body(kind, message);
        (code, body)
    };
    let route = (request.method.as_str(), request.path.as_str());
    let known_model = route.0 == "POST"
        && (matches!(route.1, "/v1/messages" | "/v1/responses") || gemini_route(route.1).is_some());
    if !known_model && !matches!(route, ("POST", "/mcp") | ("GET", "/mcp")) {
        let (code, body) = reply(404, "not_found_error", "not_found");
        return write_response(
            &mut client,
            code,
            "application/json",
            body.as_bytes(),
            cfg.write_timeout,
        )
        .await;
    }
    if route == ("GET", "/mcp") {
        // Server-initiated MCP SSE streams are intentionally unsupported.
        return write_response(
            &mut client,
            405,
            "application/json",
            b"{}",
            cfg.write_timeout,
        )
        .await;
    }
    let Some((session, gone)) = bearer(&request).and_then(|token| shared.parts.sessions.get(token))
    else {
        let (code, body) = reply(401, "authentication_error", "unauthorized");
        return write_response(
            &mut client,
            code,
            "application/json",
            body.as_bytes(),
            cfg.write_timeout,
        )
        .await;
    };
    if route.1 == "/mcp" {
        let (code, body) = mcp::handle(shared, &session, &gone, &request.body).await;
        return write_response(
            &mut client,
            code,
            "application/json",
            &body,
            cfg.write_timeout,
        )
        .await;
    }
    let protocol_matches = match session.protocol {
        Protocol::Anthropic => route.1 == "/v1/messages",
        Protocol::OpenAiResponses => route.1 == "/v1/responses",
        Protocol::Gemini => gemini_route(route.1).is_some(),
    };
    if !protocol_matches {
        let body = protocol_error_body(session.protocol, 404, "not_found_error", "not_found");
        return write_response(
            &mut client,
            404,
            "application/json",
            body.as_bytes(),
            cfg.write_timeout,
        )
        .await;
    }
    relay::model(&mut client, shared, (session, gone), request).await;
}
