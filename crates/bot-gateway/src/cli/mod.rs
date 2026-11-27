//! CLI Unix-socket adapter using OS peer UID for caller identity and line-delimited JSON framing.
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
const MAX_WAIT: Duration = Duration::from_secs(3600);
const MAX_CONNECTIONS: usize = 64;
/// Extra byte allowance for JSON envelope framing beyond `limits.body_bytes`.
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

struct Guard {
    real: Real,
    owner: Owner,
    principal: PrincipalId,
}

/// Fixed user-facing messages per error code; never echo request content.
fn message_of(code: &str) -> &'static str {
    match code {
        "invalid" => "Invalid request format",
        "too_large" => "Request is too large",
        "forbidden" => "This user cannot access this profile",
        "no_entrance" => "No entrance is mapped to this user",
        "profile_required" => "Multiple profiles available — select one with --profile",
        "rate_limited" => "Too many requests — try again shortly",
        "conflict" => "A different request already exists with this request ID",
        "not_found" => "Request not found",
        "timeout" => "Timed out waiting for result — reattach with the same request ID",
        _ => "Temporarily unavailable — retry with the same request ID",
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

async fn emit(out: &mut OwnedWriteHalf, value: Value) -> bool {
    let mut line = value.to_string();
    line.push('\n');
    out.write_all(line.as_bytes()).await.is_ok() && out.flush().await.is_ok()
}

async fn fail(out: &mut OwnedWriteHalf, code: &str, retry_after: Option<u64>) {
    fail_with(out, code, message_of(code), retry_after).await;
}

async fn fail_with(out: &mut OwnedWriteHalf, code: &str, message: &str, retry_after: Option<u64>) {
    let mut value = json!({"v": VERSION, "type": "error", "code": code, "message": message});
    if let Some(secs) = retry_after {
        value["retry_after_secs"] = json!(secs);
    }
    emit(out, value).await;
}

/// Only lists profiles reachable by this OS user so other users' profiles are never disclosed.
fn names(profiles: &[ProfileId]) -> String {
    profiles
        .iter()
        .map(|p| p.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn no_entrance_message(asked: Option<&ProfileId>, mine: &[ProfileId]) -> String {
    match (mine.first(), asked) {
        (Some(first), Some(asked)) => format!(
            "No entrance is mapped to this user for profile `{}`. Available profiles: {} — select one with `--profile {}`",
            asked.0,
            names(mine),
            first.0
        ),
        _ => "No entrance is mapped to this user. Check that the profile's CLI binding `account` is mapped to this OS user in `gateway.cli.accounts`.".to_owned(),
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
                        continue;
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
        // Identity comes solely from kernel peer credentials, never from request text.
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
                        "Multiple profiles available: {} — select one with `--profile` (e.g. `--profile {}`)",
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
                // Admit against the exact route snapshot used for authentication.
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
                // Do not reveal request existence to revoked users or emit results from stale processes.
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
            // Re-check membership and active set right before emitting status or result.
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
                // Drain any unexpected client bytes; EOF means the peer disconnected.
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
