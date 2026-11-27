//! CLI 어댑터. 로컬 유닉스 소켓으로 받고, 접속한 프로세스의 uid(피어 자격)로 OS 사용자를 확인한다 — 요청 글로는 신원을
//! 정할 수 없다. 접수 순서는 다른 입구와 같다: 신원 확인 → 정규화 → `Ingress::admit` → 응답.
//!
//! 한 연결에 요청 한 줄(UTF-8 JSON)과 응답 여러 줄이다. `v`는 프로토콜 판본(1).
//! - 요청 `{"v":1,"op":"chat","profile"?,"request","thread"?,"text","wait"?}` 또는
//!   `{"v":1,"op":"result","profile"?,"request","wait"?}`(같은 요청 ID로 결과 재조회·재부착).
//! - 응답 `accepted` → (`state`)* → `result`, 또는 `error`(`code`, 고정 문구 `message`, 선택 `retry_after_secs`).
//!
//! 요청 ID는 호출자가 정하고 재시도에도 같아야 한다(`client::new_request_id`). 연결이 끊겨도 run은 계속되고,
//! 같은 요청 ID로 다시 붙으면 기존 결과를 받는다. 회신은 outbox가 아니라 상태 조회로 전달한다.
pub mod client;
pub mod socket;

use crate::{
    active::Release,
    entrance::{Owner, Real},
    identity,
    inbound::{Event, Inbound, valid_request_id},
    ingress::{Admission, Ingress, Rejection},
};
use bot_core::{
    id::{PrincipalId, ProfileId, RequestId},
    intake::{RequestKey, RequestState},
    port::RunStore,
    profile::Adapter,
    state::{Conversation, ConversationKind, RunState},
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

pub const VERSION: u64 = 1;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 한 연결이 결과를 기다리는 최대 시간. 넘으면 끊고, 같은 요청 ID로 다시 붙으면 된다.
const MAX_WAIT: Duration = Duration::from_secs(3600);
const MAX_CONNECTIONS: usize = 64;
/// 요청 줄에서 본문 한도 외에 허용하는 JSON 틀의 여유
const FRAME_SLACK: usize = 1024;

pub struct CliGateway {
    ingress: Arc<Ingress>,
    store: Arc<dyn RunStore>,
    connections: Arc<Semaphore>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Op {
    Chat,
    Result,
}

struct Request {
    op: Op,
    profile: Option<ProfileId>,
    request: String,
    thread: Option<String>,
    text: String,
    wait: bool,
}

/// 결과·상태를 돌려주기 직전에 지금의 바인딩·멤버 권한을 다시 확인하는 데 쓰는 신원 묶음
struct Guard {
    real: Real,
    owner: Owner,
    principal: PrincipalId,
}

/// 오류 코드와 사람이 읽는 고정 문구. 요청 내용은 싣지 않는다.
fn message_of(code: &str) -> &'static str {
    match code {
        "invalid" => "요청 형식이 올바르지 않아요",
        "too_large" => "요청이 너무 커요",
        "forbidden" => "이 사용자는 이 프로필을 쓸 수 없어요",
        "no_entrance" => "이 사용자에게 연결된 입구가 없어요",
        "profile_required" => "프로필이 여러 개예요 — --profile로 골라 주세요",
        "rate_limited" => "요청이 너무 많아요 — 잠시 뒤 다시 시도해 주세요",
        "conflict" => "같은 요청 ID에 다른 내용이 왔어요",
        "not_found" => "그런 요청이 없어요",
        "timeout" => "결과를 기다리는 시간이 지났어요 — 같은 요청 ID로 다시 붙어 보세요",
        _ => "지금은 접수할 수 없어요 — 같은 요청 ID로 다시 시도해 주세요",
    }
}

fn parse(line: &str) -> Result<Request, &'static str> {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
        return Err("invalid");
    };
    if map.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Err("invalid");
    }
    let op = match map.get("op").and_then(Value::as_str) {
        Some("chat") => Op::Chat,
        Some("result") => Op::Result,
        _ => return Err("invalid"),
    };
    let text_of = |key: &str, ok: fn(&str) -> bool| match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if ok(text) => Ok(Some(text.clone())),
        _ => Err("invalid"),
    };
    let request = text_of("request", valid_request_id)?.ok_or("invalid")?;
    let thread = text_of("thread", valid_request_id)?;
    let profile = text_of("profile", identity::is_alias)?.map(ProfileId);
    let text = match (op, map.get("text")) {
        (Op::Chat, Some(Value::String(text))) if !text.trim().is_empty() => text.clone(),
        (Op::Chat, _) => return Err("invalid"),
        (Op::Result, None | Some(Value::Null)) => String::new(),
        (Op::Result, _) => return Err("invalid"),
    };
    let wait = match map.get("wait") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(wait)) => *wait,
        _ => return Err("invalid"),
    };
    Ok(Request {
        op,
        profile,
        request,
        thread,
        text,
        wait,
    })
}

enum LineError {
    TooLong,
    Closed,
}

/// 줄바꿈까지 읽되 `limit` 바이트를 넘으면 멈춘다.
async fn read_line(
    reader: &mut BufReader<OwnedReadHalf>,
    limit: usize,
) -> Result<String, LineError> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await.map_err(|_| LineError::Closed)?;
        if buffer.is_empty() {
            return Err(LineError::Closed);
        }
        match buffer.iter().position(|&b| b == b'\n') {
            Some(end) => {
                line.extend_from_slice(&buffer[..end]);
                reader.consume(end + 1);
                break;
            }
            None => {
                line.extend_from_slice(buffer);
                let taken = buffer.len();
                reader.consume(taken);
            }
        }
        if line.len() > limit {
            return Err(LineError::TooLong);
        }
    }
    if line.len() > limit {
        return Err(LineError::TooLong);
    }
    String::from_utf8(line).map_err(|_| LineError::Closed)
}

/// 한 줄을 쓴다. 쓰지 못하면(상대가 끊음) false.
async fn emit(out: &mut OwnedWriteHalf, value: Value) -> bool {
    let mut line = value.to_string();
    line.push('\n');
    out.write_all(line.as_bytes()).await.is_ok() && out.flush().await.is_ok()
}

async fn fail(out: &mut OwnedWriteHalf, code: &str, retry_after: Option<u64>) {
    fail_with(out, code, message_of(code), retry_after).await;
}

/// 고정 문구 대신 상황에 맞는 문구를 싣는 오류. `code`는 그대로라 기계가 읽는 쪽은 달라지지 않는다.
async fn fail_with(out: &mut OwnedWriteHalf, code: &str, message: &str, retry_after: Option<u64>) {
    let mut value = json!({"v": VERSION, "type": "error", "code": code, "message": message});
    if let Some(secs) = retry_after {
        value["retry_after_secs"] = json!(secs);
    }
    emit(out, value).await;
}

/// 이 OS 사용자가 CLI로 닿는 프로필 이름들(쉼표로 이은 글). 이 사용자의 입구만 담으므로 다른 사용자의 프로필은 드러나지 않는다.
fn names(profiles: &[ProfileId]) -> String {
    profiles
        .iter()
        .map(|p| p.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// 입구가 없을 때의 문구. `asked`는 사용자가 고른 프로필, `mine`은 이 사용자가 닿는 프로필들이다.
/// 닿는 프로필이 있으면 그 이름과 고르는 법을, 하나도 없으면 호스트 설정에서 확인할 곳을 알려 준다.
fn no_entrance_message(asked: Option<&ProfileId>, mine: &[ProfileId]) -> String {
    match (mine.first(), asked) {
        (Some(first), Some(asked)) => format!(
            "프로필 `{}`에는 이 사용자에게 연결된 입구가 없어요. 이 사용자가 쓸 수 있는 프로필: {} — `--profile {}`처럼 골라 주세요",
            asked.0,
            names(mine),
            first.0
        ),
        _ => "이 사용자에게 연결된 입구가 없어요. 프로필의 cli 바인딩 `account`가 호스트 설정 `gateway.cli.accounts`에서 이 OS 사용자에게 연결돼 있는지 확인해 주세요".to_owned(),
    }
}

fn state_name(state: RunState) -> &'static str {
    match state {
        RunState::Ready | RunState::Leased | RunState::Retry => "running",
        RunState::Waiting => "waiting",
        RunState::Succeeded => "succeeded",
        RunState::Dead => "dead",
        RunState::Cancelled => "cancelled",
    }
}

fn terminal(state: RunState) -> bool {
    matches!(
        state,
        RunState::Succeeded | RunState::Dead | RunState::Cancelled
    )
}

impl CliGateway {
    pub fn new(ingress: Arc<Ingress>, store: Arc<dyn RunStore>) -> Self {
        Self {
            ingress,
            store,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// stop이 오면 새 연결을 받지 않고 돌아온다(진행 중인 연결은 stop을 보고 스스로 끝낸다).
    pub async fn serve(self: Arc<Self>, listener: UnixListener, stop: CancellationToken) {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let Ok(permit) = self.connections.clone().try_acquire_owned() else {
                        continue; // 한도를 넘는 연결은 바로 닫는다
                    };
                    let gateway = self.clone();
                    let stop = stop.clone();
                    tokio::spawn(async move {
                        gateway.connection(stream, stop).await;
                        drop(permit);
                    });
                }
            }
        }
    }

    async fn connection(&self, stream: UnixStream, stop: CancellationToken) {
        // 신원은 접속한 프로세스의 uid뿐이다
        let Ok(credential) = stream.peer_cred() else {
            return;
        };
        let uid = credential.uid();
        let (read, mut out) = stream.into_split();
        let mut input = BufReader::new(read);
        let limit = self.ingress.limits().body_bytes + FRAME_SLACK;
        let line = match tokio::time::timeout(READ_TIMEOUT, read_line(&mut input, limit)).await {
            Ok(Ok(line)) => line,
            Ok(Err(LineError::TooLong)) => return fail(&mut out, "too_large", None).await,
            Ok(Err(LineError::Closed)) | Err(_) => return,
        };
        match parse(&line) {
            Ok(request) => self.handle(uid, request, &mut out, &mut input, &stop).await,
            Err(code) => fail(&mut out, code, None).await,
        }
    }

    async fn handle(
        &self,
        uid: u32,
        request: Request,
        out: &mut OwnedWriteHalf,
        input: &mut BufReader<OwnedReadHalf>,
        stop: &CancellationToken,
    ) {
        let Ok(routes) = self.ingress.routes() else {
            return fail(out, "unavailable", Some(1)).await;
        };
        let mine = routes.cli_profiles(uid);
        let profile = match request.profile.clone() {
            Some(profile) => profile,
            None => match mine.as_slice() {
                [only] => only.clone(),
                [] => {
                    let message = no_entrance_message(None, &mine);
                    return fail_with(out, "no_entrance", &message, None).await;
                }
                _ => {
                    let message = format!(
                        "프로필이 여러 개예요: {} — `--profile`로 골라 주세요(예: `--profile {}`)",
                        names(&mine),
                        mine[0].0
                    );
                    return fail_with(out, "profile_required", &message, None).await;
                }
            },
        };
        let real = Real::Cli {
            uid,
            profile: profile.clone(),
        };
        let Some(route) = routes.find(&real) else {
            let message = no_entrance_message(Some(&profile), &mine);
            return fail_with(out, "no_entrance", &message, None).await;
        };
        let (binding, source) = (route.binding_id.clone(), route.binding.source.clone());
        let Adapter::Cli { account } = source else {
            return fail(out, "no_entrance", None).await;
        };
        let Some(principal) = identity::local(&account) else {
            return fail(out, "invalid", None).await;
        };
        let guard = Guard {
            real: real.clone(),
            owner: Owner {
                profile: profile.clone(),
                binding: binding.clone(),
            },
            principal: principal.clone(),
        };
        let key = RequestKey {
            profile,
            binding,
            principal: principal.clone(),
            request: RequestId(request.request.clone()),
        };
        match request.op {
            Op::Chat => {
                let inbound = Inbound {
                    real,
                    principal,
                    request: key.request.clone(),
                    conversation: Conversation {
                        kind: ConversationKind::Cli,
                        id: account,
                        thread: Some(request.thread.unwrap_or_else(|| request.request.clone())),
                    },
                    attachments: Vec::new(),
                    body: json!({"text": request.text}),
                    event: Event::Message,
                };
                // 인증에 쓴 바로 그 표로 접수한다(그 사이 입구 소유자가 바뀌어도 같은 스냅숏으로 커밋된다)
                let admission = self.ingress.admit_with(&routes, inbound).await;
                drop(routes);
                match admission {
                    Admission::Accepted { .. } => {}
                    Admission::Rejected(rejection) => {
                        let (code, retry) = match rejection {
                            Rejection::Unregistered | Rejection::Forbidden => ("forbidden", None),
                            Rejection::RateLimited(wait) => {
                                ("rate_limited", Some(wait.as_secs() + 1))
                            }
                            Rejection::TooLarge => ("too_large", None),
                            Rejection::NoRoute | Rejection::Invalid => ("invalid", None),
                            Rejection::Conflict => ("conflict", None),
                            Rejection::Unavailable => ("unavailable", Some(1)),
                        };
                        return fail(out, code, retry).await;
                    }
                    Admission::Ignored => return fail(out, "no_entrance", None).await,
                    Admission::Private => return fail(out, "invalid", None).await,
                }
            }
            Op::Result => {
                drop(routes);
                let read = self.store.status(key.clone()).await;
                // 철회된 사용자에게는 요청이 있는지도 알리지 않고, 구판 프로세스는 결과를 내보내지 않는다
                if let Some((code, retry)) = self.withheld(&guard).await {
                    return fail(out, code, retry).await;
                }
                match read {
                    Ok(Some(_)) => {}
                    Ok(None) => return fail(out, "not_found", None).await,
                    Err(_) => return fail(out, "unavailable", Some(1)).await,
                }
            }
        }
        if !emit(
            out,
            json!({"v": VERSION, "type": "accepted", "request": request.request}),
        )
        .await
        {
            return;
        }
        if request.wait {
            self.follow(key, &guard, out, input, stop).await;
        }
    }

    /// 응답 직전 재확인. 막아야 하면 오류 코드(와 재시도 초)를 준다: 멤버가 아니면 `forbidden`, 이 프로세스의
    /// 판본이 저장소의 활성 판본이 아니면(구판) `unavailable`이다.
    async fn withheld(&self, guard: &Guard) -> Option<(&'static str, Option<u64>)> {
        match self
            .ingress
            .release(&guard.real, &guard.owner, &guard.principal)
            .await
        {
            Release::Allowed => None,
            Release::Forbidden => Some(("forbidden", None)),
            Release::Unavailable => Some(("unavailable", Some(1))),
        }
    }

    /// 끝날 때까지 상태를 지켜보며 상태가 바뀔 때마다 알린다. 상대가 끊으면 멈춘다(run은 계속된다).
    async fn follow(
        &self,
        key: RequestKey,
        guard: &Guard,
        out: &mut OwnedWriteHalf,
        input: &mut BufReader<OwnedReadHalf>,
        stop: &CancellationToken,
    ) {
        let started = tokio::time::Instant::now();
        let mut delay = Duration::from_millis(50);
        let mut last: Option<&'static str> = None;
        loop {
            let read = self.store.status(key.clone()).await;
            // 읽는 사이·기다리는 동안 권한이 철회됐을 수 있다 — 상태·결과를 내보내기 직전에 다시 확인한다
            if let Some((code, retry)) = self.withheld(guard).await {
                return fail(out, code, retry).await;
            }
            match read {
                Ok(Some(RequestState { state, result, .. })) => {
                    if terminal(state) {
                        let value = json!({"v": VERSION, "type": "result", "state": state_name(state),
                            "result": result.and_then(|r| serde_json::from_str::<Value>(&r.0).ok())});
                        emit(out, value).await;
                        return;
                    }
                    let name = state_name(state);
                    if last != Some(name) {
                        last = Some(name);
                        if name == "waiting"
                            && !emit(out, json!({"v": VERSION, "type": "state", "state": name}))
                                .await
                        {
                            return;
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => return fail(out, "unavailable", Some(1)).await,
            }
            if started.elapsed() >= MAX_WAIT {
                return fail(out, "timeout", None).await;
            }
            let disconnected = async {
                // 상대가 더 보내는 바이트는 버리고, 닫히면(0바이트) 끊긴 것이다
                loop {
                    match input.fill_buf().await {
                        Ok([]) | Err(_) => return,
                        Ok(bytes) => {
                            let taken = bytes.len();
                            input.consume(taken);
                        }
                    }
                }
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
                _ = disconnected => return,
            }
            delay = (delay * 2).min(Duration::from_secs(1));
        }
    }
}
