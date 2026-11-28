//! Local fake Slack Web API and Socket Mode server for integration tests.
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

#[derive(Clone, Debug)]
pub struct Call {
    pub method: String,
    pub token: String,
    pub body: Value,
    pub reply: Value,
    pub status: u16,
}

impl Call {
    pub fn succeeded(&self) -> bool {
        (200..300).contains(&self.status) && self.reply["ok"] == true
    }

    /// Returns confirmed `(channel, ts)` using the same post-confirmation rule as production delivery.
    pub fn posted(&self) -> Option<(String, String)> {
        if self.method != "chat.postMessage" || !self.succeeded() {
            return None;
        }
        crate::slack::send::confirms_post(self.body["channel"].as_str()?, &self.reply)
    }
}

pub enum Scripted {
    Json(Value),
    Raw {
        status: u16,
        retry_after: Option<u64>,
        body: String,
    },
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
    revoked: Mutex<HashMap<String, String>>,
    uploads: Mutex<Vec<Vec<u8>>>,
    acks: Mutex<Vec<String>>,
    connection: Mutex<Option<mpsc::UnboundedSender<Push>>>,
    connects: AtomicUsize,
    hello_app: Mutex<Option<String>>,
    team: Mutex<String>,
    bot_user: Mutex<String>,
    url: Mutex<Option<String>>,
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
        "auth.test" => {
            let mut reply = json!({"ok": true, "user_id": shared.bot_user.lock().unwrap().clone(),
                "team_id": shared.team.lock().unwrap().clone()});
            if let Some(url) = shared.url.lock().unwrap().clone() {
                reply["url"] = json!(url);
            }
            reply
        }
        // Maps user `U...` to deterministic DM channel `D...` for tests.
        "conversations.open" => match request["users"].as_str().filter(|users| users.len() > 1) {
            Some(users) => json!({"ok": true, "channel": {"id": format!("D{}", &users[1..])}}),
            None => json!({"ok": false, "error": "users_list_not_supplied"}),
        },
        "chat.postMessage" | "chat.startStream" => {
            json!({"ok": true, "channel": request["channel"],
            "ts": format!("1700000000.{n:06}")})
        }
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
            url: Mutex::new(None),
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

    pub fn script(&self, method: &str, reply: Scripted) {
        self.shared
            .scripts
            .lock()
            .unwrap()
            .entry(method.to_owned())
            .or_default()
            .push_back(reply);
    }

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

    pub fn set_url(&self, url: Option<&str>) {
        *self.shared.url.lock().unwrap() = url.map(str::to_owned);
    }

    pub async fn until(&self, mut condition: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..500 {
            if condition(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        condition(self)
    }

    pub async fn connected(&self) -> bool {
        self.until(|s| s.shared.connection.lock().unwrap().is_some())
            .await
    }

    pub async fn push(&self, frame: Value) {
        assert!(self.connected().await, "socket did not connect");
        let sender = self.shared.connection.lock().unwrap().clone().unwrap();
        let _ = sender.send(Push::Frame(frame.to_string()));
    }

    pub fn drop_connection(&self) {
        if let Some(sender) = self.shared.connection.lock().unwrap().take() {
            let _ = sender.send(Push::Close);
        }
    }
}

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
