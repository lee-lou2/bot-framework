//! 가짜 Slack 서버(시험용): Web API(axum)와 Socket Mode 웹소켓. 받은 호출을 기록하고, 메서드별로 응답을 바꿔 끼울 수 있다.
//! 실제 Slack에는 접속하지 않는다.
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// 기록된 Web API 호출 하나. 본문은 JSON이면 그대로, 폼이면 키-값 객체다. `reply`는 가짜 서버가 돌려준 JSON 응답이다(JSON이 아닌 응답은 `Null`).
#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub token: String,
    pub body: Value,
    pub reply: Value,
    /// 가짜 서버가 돌려준 HTTP 상태
    pub status: u16,
}

impl Call {
    /// 성공(2xx이고 `ok: true`)으로 응답한 호출인가. 거절·오류 응답이나 JSON이 아닌 응답은 아니다.
    pub fn succeeded(&self) -> bool {
        (200..300).contains(&self.status) && self.reply["ok"] == true
    }

    /// 운영 발송기가 이 `chat.postMessage` 호출을 «올라갔다»로 확정할 만한가: 정상 HTTP·`ok: true`·응답 채널이 요청 채널과 같음·유효한 `ts`·잘림 경고 없음.
    /// 확정하면 `(채널, ts)`다. 승인 버튼을 찾는 도우미가 거절·어긋난·불명 응답의 카드를 고르지 않게 한다.
    pub fn posted(&self) -> Option<(String, String)> {
        if self.method != "chat.postMessage" || !self.succeeded() {
            return None;
        }
        crate::slack::send::confirms_post(self.body["channel"].as_str()?, &self.reply)
    }
}

/// 메서드의 다음 응답을 이 값으로 바꾼다(한 번 쓰고 사라진다).
pub enum Scripted {
    Json(Value),
    Raw {
        status: u16,
        retry_after: Option<u64>,
        body: String,
    },
    /// `delay`만큼 뜸을 들인 뒤 이 JSON으로 답한다(시간 초과 시험).
    Delayed {
        delay: std::time::Duration,
        body: Value,
    },
}

enum Push {
    Frame(String),
    Close,
}

struct Shared {
    calls: Mutex<Vec<Call>>,
    scripts: Mutex<HashMap<String, VecDeque<Scripted>>>,
    /// 거절하는 토큰 → 돌려줄 오류 코드(`invalid_auth` 등). 어떤 메서드든 이 토큰으로 오면 실행하지 않고 거절한다.
    revoked: Mutex<HashMap<String, String>>,
    uploads: Mutex<Vec<Vec<u8>>>,
    acks: Mutex<Vec<String>>,
    connection: Mutex<Option<mpsc::UnboundedSender<Push>>>,
    connects: AtomicUsize,
    hello_app: Mutex<Option<String>>,
    team: Mutex<String>,
    bot_user: Mutex<String>,
    /// 사용자 ID → `users.info`의 `user` 객체에 덧씌울 필드(시험이 «봇이다»·«비활성이다»를 정한다)
    users: Mutex<HashMap<String, Value>>,
    counter: AtomicU64,
    base: Mutex<String>,
    ws_url: Mutex<String>,
}

pub struct FakeSlack {
    pub base: String,
    shared: Arc<Shared>,
    stop: CancellationToken,
}

impl Drop for FakeSlack {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn decode_form(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 2;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let text = String::from_utf8_lossy(body);
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        return serde_json::from_str(&text).unwrap_or(Value::Null);
    }
    let mut map = serde_json::Map::new();
    for pair in text.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(decode_form(key), Value::String(decode_form(value)));
    }
    Value::Object(map)
}

async fn api(
    State(shared): State<Arc<Shared>>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    let parsed = parse_body(&headers, &body);
    // 거절된 토큰은 실제 Slack처럼 인증 단계에서 막는다(대본·기본 응답까지 가지 않는다)
    let refused = shared.revoked.lock().unwrap().get(&token).cloned();
    let scripted = match refused {
        Some(error) => Some(Scripted::Json(json!({"ok": false, "error": error}))),
        None => shared
            .scripts
            .lock()
            .unwrap()
            .get_mut(&method)
            .and_then(VecDeque::pop_front),
    };
    // 응답을 먼저 정하고 기록한다(기록에 응답이 함께 남는다). 기록은 응답을 내보내기 전에 끝난다.
    let (response, reply, status) = match scripted {
        Some(Scripted::Json(value)) => (axum::Json(value.clone()).into_response(), value, 200),
        Some(Scripted::Delayed { delay, body }) => {
            tokio::time::sleep(delay).await;
            (axum::Json(body.clone()).into_response(), body, 200)
        }
        Some(Scripted::Raw {
            status,
            retry_after,
            body,
        }) => {
            let reply = serde_json::from_str(&body).unwrap_or(Value::Null);
            let mut response = (StatusCode::from_u16(status).unwrap(), body).into_response();
            if let Some(secs) = retry_after {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, secs.into());
            }
            (response, reply, status)
        }
        None => {
            let value = default_reply(&shared, &method, &parsed);
            (axum::Json(value.clone()).into_response(), value, 200)
        }
    };
    shared.calls.lock().unwrap().push(Call {
        method,
        token,
        body: parsed,
        reply,
        status,
    });
    response
}

fn default_reply(shared: &Shared, method: &str, request: &Value) -> Value {
    let n = shared.counter.fetch_add(1, Ordering::SeqCst) + 1;
    match method {
        "apps.connections.open" => {
            json!({"ok": true, "url": shared.ws_url.lock().unwrap().clone()})
        }
        "auth.test" => json!({"ok": true, "user_id": shared.bot_user.lock().unwrap().clone(),
            "team_id": shared.team.lock().unwrap().clone()}),
        // 실제 Slack처럼 게시된 채널을 되돌려 준다(영수증은 응답이 말한 곳으로 확인한다)
        // 사용자 `U1`의 1:1 DM은 `D1`이다(시험이 쓰는 대응: 첫 글자만 `D`로 바꾼다)
        "conversations.open" => match request["users"].as_str().filter(|users| users.len() > 1) {
            Some(users) => json!({"ok": true, "channel": {"id": format!("D{}", &users[1..])}}),
            None => json!({"ok": false, "error": "users_list_not_supplied"}),
        },
        "chat.postMessage" | "chat.startStream" => {
            json!({"ok": true, "channel": request["channel"],
            "ts": format!("1700000000.{n:06}")})
        }
        // 기본은 활성 사람 계정이다. 시험이 `set_user`로 필드를 덧씌운다(봇·비활성·모양이 이상한 응답)
        "users.info" => {
            let id = request["user"].as_str().unwrap_or_default();
            let mut user = json!({"id": id, "team_id": shared.team.lock().unwrap().clone(),
                "deleted": false, "is_bot": false, "is_app_user": false});
            if let (Some(base), Some(Value::Object(over))) =
                (user.as_object_mut(), shared.users.lock().unwrap().get(id))
            {
                base.extend(over.clone());
            }
            json!({"ok": true, "user": user})
        }
        // 업로드 완료는 올라간 파일들을 되돌려 준다
        "files.completeUploadExternal" => {
            let files: Vec<Value> = request["files"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Vec<Value>>(text).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|file| json!({"id": file["id"], "title": file["title"]}))
                .collect();
            json!({"ok": true, "files": files})
        }
        "chat.postEphemeral" => json!({"ok": true, "message_ts": format!("1700000000.{n:06}")}),
        "files.getUploadURLExternal" => {
            let base = shared.base.lock().unwrap().clone();
            json!({"ok": true, "upload_url": format!("{}upload/{n}", base.trim_end_matches("api/")),
                "file_id": format!("F{n:04}")})
        }
        _ => json!({"ok": true}),
    }
}

async fn upload(
    State(shared): State<Arc<Shared>>,
    Path(_id): Path<String>,
    body: Bytes,
) -> Response {
    shared.uploads.lock().unwrap().push(body.to_vec());
    (StatusCode::OK, "OK - 1").into_response()
}

async fn socket_server(shared: Arc<Shared>, listener: TcpListener, stop: CancellationToken) {
    loop {
        let stream = tokio::select! {
            _ = stop.cancelled() => return,
            accepted = listener.accept() => match accepted { Ok((s, _)) => s, Err(_) => continue },
        };
        let shared = shared.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (tx, mut rx) = mpsc::unbounded_channel();
            *shared.connection.lock().unwrap() = Some(tx);
            shared.connects.fetch_add(1, Ordering::SeqCst);
            let hello = match shared.hello_app.lock().unwrap().clone() {
                Some(app) => json!({"type": "hello", "connection_info": {"app_id": app}}),
                None => json!({"type": "hello"}),
            };
            let _ = ws.send(Message::Text(hello.to_string().into())).await;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    push = rx.recv() => match push {
                        Some(Push::Frame(text)) => { let _ = ws.send(Message::Text(text.into())).await; }
                        Some(Push::Close) | None => { let _ = ws.close(None).await; return; }
                    },
                    incoming = ws.next() => match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(value) = serde_json::from_str::<Value>(&text)
                                && let Some(id) = value["envelope_id"].as_str()
                            {
                                shared.acks.lock().unwrap().push(id.to_owned());
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => return,
                    },
                }
            }
        });
    }
}

impl FakeSlack {
    pub async fn start() -> Self {
        let shared = Arc::new(Shared {
            calls: Mutex::default(),
            scripts: Mutex::default(),
            revoked: Mutex::default(),
            uploads: Mutex::default(),
            acks: Mutex::default(),
            connection: Mutex::default(),
            connects: AtomicUsize::new(0),
            hello_app: Mutex::new(Some("A1".into())),
            team: Mutex::new("T1".into()),
            bot_user: Mutex::new("UBOT".into()),
            users: Mutex::default(),
            counter: AtomicU64::new(0),
            base: Mutex::default(),
            ws_url: Mutex::default(),
        });
        let stop = CancellationToken::new();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/", http.local_addr().unwrap());
        *shared.base.lock().unwrap() = base.clone();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        *shared.ws_url.lock().unwrap() = format!("ws://{}/", ws.local_addr().unwrap());
        let app = Router::new()
            .route("/api/{method}", post(api))
            .route("/upload/{id}", post(upload))
            .with_state(shared.clone());
        let child = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(http, app)
                .with_graceful_shutdown(async move { child.cancelled().await })
                .await;
        });
        tokio::spawn(socket_server(shared.clone(), ws, stop.clone()));
        Self { base, shared, stop }
    }

    pub fn calls(&self, method: &str) -> Vec<Call> {
        self.shared
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == method)
            .cloned()
            .collect()
    }

    pub fn all_calls(&self) -> Vec<Call> {
        self.shared.calls.lock().unwrap().clone()
    }

    /// 이 메서드의 다음 응답을 바꿔 끼운다(여러 번 부르면 차례로 쓴다).
    pub fn script(&self, method: &str, reply: Scripted) {
        self.shared
            .scripts
            .lock()
            .unwrap()
            .entry(method.to_owned())
            .or_default()
            .push_back(reply);
    }

    /// 이 토큰으로 오는 모든 호출을 `error`(예: `invalid_auth`·`token_revoked`·`token_expired`)로 거절한다 — 보관소에서 토큰을 바꿨는데 옛 토큰이 아직 쓰이는 상황.
    pub fn revoke(&self, token: &str, error: &str) {
        self.shared
            .revoked
            .lock()
            .unwrap()
            .insert(token.to_owned(), error.to_owned());
    }

    pub fn uploads(&self) -> Vec<Vec<u8>> {
        self.shared.uploads.lock().unwrap().clone()
    }

    pub fn acks(&self) -> Vec<String> {
        self.shared.acks.lock().unwrap().clone()
    }

    pub fn connects(&self) -> usize {
        self.shared.connects.load(Ordering::SeqCst)
    }

    pub fn set_hello_app(&self, app: Option<&str>) {
        *self.shared.hello_app.lock().unwrap() = app.map(str::to_owned);
    }

    /// 이 사용자의 `users.info` 응답에 덧씌울 필드를 정한다(다시 부르면 바꾼다). 예: `{"is_bot": true}`·`{"deleted": true}`.
    pub fn set_user(&self, id: &str, fields: Value) {
        self.shared
            .users
            .lock()
            .unwrap()
            .insert(id.to_owned(), fields);
    }

    pub fn set_auth(&self, team: &str, bot_user: &str) {
        *self.shared.team.lock().unwrap() = team.to_owned();
        *self.shared.bot_user.lock().unwrap() = bot_user.to_owned();
    }

    /// 조건이 참이 될 때까지(최대 5초) 기다린다.
    pub async fn until(&self, mut condition: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..500 {
            if condition(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        condition(self)
    }

    /// 소켓이 연결될 때까지 기다린다.
    pub async fn connected(&self) -> bool {
        self.until(|s| s.shared.connection.lock().unwrap().is_some())
            .await
    }

    /// 연결된 소켓으로 프레임을 보낸다.
    pub async fn push(&self, frame: Value) {
        assert!(self.connected().await, "소켓이 연결되지 않았어요");
        let sender = self.shared.connection.lock().unwrap().clone().unwrap();
        // 연결이 이미 닫혔으면 프레임은 사라진다(시험이 그 결과를 본다)
        let _ = sender.send(Push::Frame(frame.to_string()));
    }

    /// 서버 쪽에서 연결을 닫는다(재접속 시험).
    pub fn drop_connection(&self) {
        if let Some(sender) = self.shared.connection.lock().unwrap().take() {
            let _ = sender.send(Push::Close);
        }
    }
}

/// `events_api` 프레임(채널 멘션)
pub fn mention_frame(
    envelope: &str,
    team: &str,
    channel: &str,
    user: &str,
    ts: &str,
    text: &str,
) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": team, "event_id": format!("Ev-{envelope}"),
        "event": {"type": "app_mention", "user": user, "text": text, "ts": ts, "channel": channel}}})
}

/// `events_api` 프레임(DM)
pub fn dm_frame(
    envelope: &str,
    team: &str,
    channel: &str,
    user: &str,
    ts: &str,
    text: &str,
) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": team, "event_id": format!("Ev-{envelope}"),
        "event": {"type": "message", "channel_type": "im", "user": user, "text": text, "ts": ts, "channel": channel}}})
}

/// `interactive` 프레임(버튼 클릭)
pub fn button_frame(
    envelope: &str,
    team: &str,
    channel: &str,
    user: &str,
    message_ts: &str,
    action: &str,
    value: &str,
) -> Value {
    json!({"type": "interactive", "envelope_id": envelope, "payload": {
        "type": "block_actions", "team": {"id": team}, "user": {"id": user}, "channel": {"id": channel},
        "message": {"ts": message_ts}, "actions": [{"action_id": action, "value": value}]}})
}

/// `events_api` 프레임(채널 글: `message.channels`). `thread_ts`가 있으면 스레드 안의 글이다.
pub fn post_frame(
    envelope: &str,
    (team, channel): (&str, &str),
    user: &str,
    ts: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> Value {
    let mut event = json!({"type": "message", "channel_type": "channel", "user": user,
        "text": text, "ts": ts, "channel": channel});
    if let Some(root) = thread_ts {
        event["thread_ts"] = json!(root);
    }
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": team, "event_id": format!("Ev-{envelope}"), "event": event}})
}

/// `events_api` 프레임(채널 글에 단 반응: `reaction_added`)
pub fn reaction_frame(
    envelope: &str,
    (team, channel): (&str, &str),
    user: &str,
    item_ts: &str,
    reaction: &str,
    item_user: &str,
) -> Value {
    json!({"type": "events_api", "envelope_id": envelope, "payload": {
        "team_id": team, "event_id": format!("Ev-{envelope}"),
        "event": {"type": "reaction_added", "user": user, "reaction": reaction, "item_user": item_user,
            "item": {"type": "message", "channel": channel, "ts": item_ts}, "event_ts": "9999999999.000100"}}})
}
