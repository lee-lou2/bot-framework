//! 모델 샌드박스가 닿는 유일한 HTTP 입구. 호스트 전용 망(또는 루프백)의 한 주소에서 받는다.
//!
//! 실제 Claude Code CLI로 확인한 통로(2.1.331): 모델은 `POST /v1/messages?beta=true`(SSE 스트리밍, 세션 토큰은 `x-api-key`),
//! 도구는 MCP streamable HTTP(`POST /mcp`, `Authorization: Bearer <세션 토큰>`, `server/discover` → `initialize` →
//! `tools/list` → `tools/call`). 둘 다 같은 세션 토큰 하나로 받는다.
//!
//! - 세션 토큰은 run(시도)마다 호스트가 발급한다. 토큰은 해시로만 보관하고 로그·오류·Debug에 싣지 않는다.
//!   요청마다 브로커가 run·세대·claim을 원장에 대조하므로 인계되거나 끝난 run의 토큰은 쓸 수 없다.
//! - 모델 별칭·에이전트·호출 ID·주체는 샌드박스가 정하지 못한다. 별칭은 세션이 고정하고, 호출 ID는
//!   CLI의 `toolUseId`에서 정해 같은 호출이 재개돼도 같다.
//! - 요청 한 건당 연결 하나(`Connection: close`). 오류 본문은 고정 어휘만 담는다.

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

use crate::{canon::sha256, model::ModelProxy, sys::Entropy, text::hex};
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

/// 호스트가 run마다 정하는 세션. `attempt`는 실행의 현재 claim을 쥔 시도다.
#[derive(Clone)]
pub struct Session {
    pub attempt: Attempt,
    pub agent: AgentId,
    /// 실행 에이전트의 모델 별칭.
    pub alias: String,
}

/// 발급한 세션과, 거둘 때 열린 연결에 알리는 신호(보낸 쪽이 사라져도 받는 쪽은 끝난다).
struct Live {
    session: Session,
    gone: watch::Sender<bool>,
}

/// 토큰의 해시로 세션을 찾는다.
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

    /// 새 토큰을 발급한다. 난수를 못 얻으면 `None`.
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

    /// 토큰을 거두고, 이 토큰으로 열린 스트림에 알려 곧바로 끝내게 한다.
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

    /// 이 시도(프로필·run·세대·claim이 모두 같은 것)가 발급받은 토큰만 거둔다. 새 claim으로 넘어간 run의 토큰은 건드리지 않는다
    /// (옛 워커가 정상 종료하며 새 워커의 세션을 끊으면 안 된다).
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

    /// 세션과 거둠 신호. 신호는 받은 뒤에 거둬져도 알아챈다.
    fn get(&self, token: &str) -> Option<(Session, watch::Receiver<bool>)> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&token_key(token))
            .map(|live| (live.session.clone(), live.gone.subscribe()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrontConfig {
    pub head_timeout: Duration,
    pub body_timeout: Duration,
    pub write_timeout: Duration,
    pub max_connections: usize,
    pub max_body: usize,
    /// 열린 모델 요청이 있는 동안 원장에 현재 claim인지 다시 묻는 주기.
    pub revoke_check: Duration,
    /// 원장이 답하지 못하는(오류·시간 초과) 동안 출력을 멈추고 기다리는 한도. 넘기면 연결을 닫는다.
    pub verify_grace: Duration,
    /// 완료한 응답의 도구 목록을 저장하는 데 기다리는 한도(소유권 감시와 별개로 저장소가 멎었을 때를 끊는다).
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

/// 모델 CLI의 도구 호출(`toolUseId`·도구 이름·정규 인자)을 브로커의 논리 호출 ID로 바꾸는 원천. 같은 도구 사용은 재개·재전달돼도
/// 늘 같은 호출 ID여야 중복 실행이 없다. 저장된 계획과 맞지 않거나 확인할 수 없으면 오류를 내고 호출은 실행하지 않는다.
/// 운영 구현은 `cli::PlannedCalls`(저장된 모델 차례의 계획과 대조)다.
pub trait CallBook: Send + Sync {
    /// `name`은 CLI가 부른 도구 이름, `args`는 정규화한 인자다.
    fn call_id(
        &self,
        attempt: Attempt,
        tool_use: String,
        name: String,
        args: JsonPayload,
    ) -> Reply<'_, CallId>;

    /// 이 호출 위치에 이미 남은 확정 거절 표식. 프런트가 브로커를 부르기 **전에** 먼저 읽어 같은 단어로 재생한다(재전달·재개가 같은 자리를 거절과 실행으로
    /// 둘 다 끝낼 수 없다). 읽지 못하면 오류이고 호출하지 않는다. 기본은 없다: 계획과 대조하지 않는 원천은 표식을 쓰지 않는다.
    fn marked(&self, _: Attempt, _: CallId) -> Reply<'_, Option<bot_core::cli::CliRefusal>> {
        Box::pin(async { Ok(None) })
    }

    /// 브로커(또는 위임)가 `expected`로 거절한 호출이 **검증된 CLI 위치의 실행 전 확정 거절**인가. 확정이면 `true`다(그러면 프런트가 run 체크포인트에
    /// 표식을 남긴 뒤에만 그 단어를 돌려준다). 확인할 수 없으면 오류이고 표식은 없다. `expected`는 오류에서 정한다: `Missing`→`UnknownTool`,
    /// `Invalid`→`Invalid`, `Denied`·`Conflict`·`Stale`→같은 이름, `Unavailable`→`RateLimited`(부른 쪽이 **이 호출이 한도에 걸렸다**고 결과와 함께 확인했을 때만 묻는다. 저장소 장애는 묻지 않는다).
    /// 기본은 아니다: 계획과 대조하지 않는 원천은 호출 위치를 믿을 수 없다.
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

/// 시험·단순 구성용 원천: `m-<toolUseId>`. 상태가 없고 계획과 대조하지 않는다(운영 CLI 실행에는 쓰지 않는다).
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
    /// 호출 ID 원천. 기본은 `DirectIds`.
    pub calls: Arc<dyn CallBook>,
    /// 완료한 모델 응답의 도구 목록을 알릴 곳. 쓰지 못하면 그 응답은 CLI에 내보내지 않는다. 기본은 `NoSink`.
    pub turns: Arc<dyn observe::TurnSink>,
    /// 내장 `delegate`가 자식을 시작하고 자식 결과를 읽는 데 쓰는 원장·위임 저장소.
    pub ledger: Arc<dyn CallLedger>,
    pub delegation: Arc<dyn DelegationStore>,
    /// 대기에 들어간 시도를 호스트에 알리는 곳. 기본은 `NoPark`(세션 회수만 한다).
    pub park: Arc<dyn park::Park>,
}

struct Shared {
    parts: FrontParts,
    cfg: FrontConfig,
    /// 같은 호출 위치의 요청을 한 번에 하나씩 처리하는 문.
    slots: slots::Slots,
}

pub struct ModelFront {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
    #[cfg(test)]
    shared: Arc<Shared>,
}

/// 고정 어휘 오류 본문(Anthropic 형식). 요청·내부 사정은 싣지 않는다.
pub(crate) fn error_body(kind: &str, message: &str) -> String {
    format!("{{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{message}\"}}}}")
}

/// 브로커 오류 → HTTP 상태와 Anthropic 오류 종류. 인계된 run(Stale)은 CLI가 재시도하지 않게 403이다.
fn http_error(error: &Error) -> (u16, &'static str, &'static str) {
    match error {
        Error::Invalid | Error::Conflict => (400, "invalid_request_error", "invalid"),
        Error::Denied => (403, "permission_error", "denied"),
        Error::Stale => (403, "permission_error", "stale"),
        Error::Missing => (404, "not_found_error", "missing"),
        Error::Unavailable => (503, "overloaded_error", "unavailable"),
    }
}

fn bearer(request: &Request) -> Option<&str> {
    request
        .header("x-api-key")
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

    /// 새 연결을 받지 않고 진행 중인 연결도 끝낸다.
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
    if !matches!(
        route,
        ("POST", "/v1/messages") | ("POST", "/mcp") | ("GET", "/mcp")
    ) {
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
        // 서버가 먼저 보내는 SSE 채널은 쓰지 않는다.
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
    relay::model(&mut client, shared, (session, gone), request).await;
}
