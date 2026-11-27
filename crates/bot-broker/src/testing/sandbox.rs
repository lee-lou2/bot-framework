//! In-memory fake sandbox, MCP stdio server over duplex pipes, and profile-namespaced secret store.

use crate::{lines::read_line, sandbox::*};
use bot_core::{
    id::*,
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncWriteExt, BufReader, duplex},
    task::JoinHandle,
};

use super::lock;

pub enum Answer {
    Result(Value),
    Error(i64),
    Hang,
    Close,
    /// Emits raw JSON-RPC lines (server notifications/requests) before answering.
    Before(Vec<String>, Box<Answer>),
}

pub struct CallCtx {
    pub env: Vec<(String, String)>,
    pub params: Value,
    pub effects: Option<Pipe>,
}

pub type CallFuture = Pin<Box<dyn Future<Output = Answer> + Send>>;
type OnCall = Arc<dyn Fn(CallCtx) -> CallFuture + Send + Sync>;

#[derive(Clone)]
pub struct Server {
    pub version: String,
    pub tools: Vec<(String, Value)>,
    on_call: OnCall,
    stall: Stall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stall {
    Never,
    AfterList,
    BeforeInitialize,
}

impl Server {
    pub fn new(tools: &[(&str, Value)]) -> Self {
        Self {
            version: "2025-06-18".into(),
            tools: tools
                .iter()
                .map(|(n, s)| (n.to_string(), s.clone()))
                .collect(),
            stall: Stall::Never,
            on_call: Arc::new(|_| {
                Box::pin(async {
                    Answer::Result(json!({"content": [{"type": "text", "text": "ok"}]}))
                })
            }),
        }
    }

    pub fn stalling_after_list(mut self) -> Self {
        self.stall = Stall::AfterList;
        self
    }

    pub fn stalling_before_initialize(mut self) -> Self {
        self.stall = Stall::BeforeInitialize;
        self
    }

    pub fn version(mut self, version: &str) -> Self {
        self.version = version.into();
        self
    }

    pub fn answering(mut self, f: impl Fn(CallCtx) -> CallFuture + Send + Sync + 'static) -> Self {
        self.on_call = Arc::new(f);
        self
    }

    pub fn replying(self, make: impl Fn() -> Answer + Send + Sync + 'static) -> Self {
        self.answering(move |_| {
            let answer = make();
            Box::pin(async move { answer })
        })
    }
}

#[derive(Default)]
pub struct Recorder {
    pub calls: Mutex<Vec<Value>>,
    pub raw_calls: Mutex<Vec<String>>,
    pub methods: Mutex<Vec<String>>,
    pub client_replies: Mutex<Vec<Value>>,
}

async fn send(write: &mut BoxWrite, value: &Value) -> bool {
    let mut line = value.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await.is_ok() && write.flush().await.is_ok()
}

pub async fn serve_mcp(
    server: Server,
    env: Vec<(String, String)>,
    mut effects: Option<Pipe>,
    read: BoxRead,
    mut write: BoxWrite,
    recorder: Arc<Recorder>,
) {
    let mut read = BufReader::new(read);
    if server.stall == Stall::BeforeInitialize {
        return std::future::pending().await;
    }
    while let Ok(Some(line)) = read_line(&mut read, 1 << 20).await {
        let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            lock(&recorder.client_replies).push(frame);
            continue;
        };
        lock(&recorder.methods).push(method.to_owned());
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let ok = match method {
            "initialize" => {
                let result = json!({"protocolVersion": server.version, "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}});
                send(&mut write, &reply(result)).await
            }
            "tools/list" => {
                let tools: Vec<Value> = server
                    .tools
                    .iter()
                    .map(|(name, schema)| json!({"name": name, "description": "x", "inputSchema": schema}))
                    .collect();
                let sent = send(&mut write, &reply(json!({"tools": tools}))).await;
                if server.stall == Stall::AfterList {
                    return std::future::pending().await;
                }
                sent
            }
            "tools/call" => {
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                lock(&recorder.calls).push(params.clone());
                lock(&recorder.raw_calls).push(String::from_utf8_lossy(&line).into_owned());
                let ctx = CallCtx {
                    env: env.clone(),
                    params,
                    effects: effects.take(),
                };
                let mut answer = (server.on_call)(ctx).await;
                let mut alive = true;
                while let Answer::Before(lines, next) = answer {
                    for text in lines {
                        let _ = write.write_all(format!("{text}\n").as_bytes()).await;
                    }
                    answer = *next;
                }
                match answer {
                    Answer::Result(result) => alive = send(&mut write, &reply(result)).await,
                    Answer::Error(code) => {
                        let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": "server detail that must not leak"}});
                        alive = send(&mut write, &error).await;
                    }
                    Answer::Hang => std::future::pending::<()>().await,
                    Answer::Close => return,
                    Answer::Before(..) => unreachable!(),
                }
                alive
            }
            "notifications/initialized" => true,
            _ => {
                let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}});
                send(&mut write, &error).await
            }
        };
        if !ok {
            return;
        }
    }
}

struct Inner {
    specs: Mutex<Vec<SandboxSpec>>,
    server: Mutex<Server>,
    recorder: Arc<Recorder>,
    terminated: Mutex<Vec<String>>,
    reaped: Mutex<Vec<String>>,
    reap_ok: Mutex<bool>,
    spawn_fails: Mutex<bool>,
    refuse_admit: Mutex<bool>,
    effects_pipe: Mutex<bool>,
    terminate: Mutex<Terminate>,
    via_proxy: Mutex<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Terminate {
    Confirmed,
    Fails,
    Hangs,
}

pub struct FakeSandbox {
    inner: Arc<Inner>,
}

impl FakeSandbox {
    pub fn new(server: Server) -> Self {
        Self {
            inner: Arc::new(Inner {
                specs: Mutex::default(),
                server: Mutex::new(server),
                recorder: Arc::new(Recorder::default()),
                terminated: Mutex::default(),
                reaped: Mutex::default(),
                reap_ok: Mutex::new(true),
                spawn_fails: Mutex::new(false),
                refuse_admit: Mutex::new(false),
                effects_pipe: Mutex::new(true),
                terminate: Mutex::new(Terminate::Confirmed),
                via_proxy: Mutex::new(false),
            }),
        }
    }
    pub fn set_server(&self, server: Server) {
        *lock(&self.inner.server) = server;
    }
    pub fn specs(&self) -> Vec<SandboxSpec> {
        lock(&self.inner.specs).clone()
    }
    pub fn calls(&self) -> Vec<Value> {
        lock(&self.inner.recorder.calls).clone()
    }
    pub fn raw_calls(&self) -> Vec<String> {
        lock(&self.inner.recorder.raw_calls).clone()
    }
    pub fn methods(&self) -> Vec<String> {
        lock(&self.inner.recorder.methods).clone()
    }
    pub fn client_replies(&self) -> Vec<Value> {
        lock(&self.inner.recorder.client_replies).clone()
    }
    pub fn terminated(&self) -> Vec<String> {
        lock(&self.inner.terminated).clone()
    }
    pub fn reaped(&self) -> Vec<String> {
        lock(&self.inner.reaped).clone()
    }
    /// Simulates an FD-less sandbox (like `srt`) that tunnels effect callbacks through the proxy endpoint.
    pub fn set_effects_via_proxy(&self, on: bool) {
        *lock(&self.inner.via_proxy) = on;
    }
    pub fn set_terminate(&self, mode: Terminate) {
        *lock(&self.inner.terminate) = mode;
    }
    pub fn set_reap_ok(&self, ok: bool) {
        *lock(&self.inner.reap_ok) = ok;
    }
    pub fn fail_spawn(&self, fail: bool) {
        *lock(&self.inner.spawn_fails) = fail;
    }
    pub fn set_refuse_admit(&self, refuse: bool) {
        *lock(&self.inner.refuse_admit) = refuse;
    }
    pub fn withhold_effects_pipe(&self, withhold: bool) {
        *lock(&self.inner.effects_pipe) = !withhold;
    }
}

struct FakeHandle {
    inner: Arc<Inner>,
    owner: String,
    task: JoinHandle<()>,
}

impl Handle for FakeHandle {
    fn terminate(self: Box<Self>) -> Reply<'static, ()> {
        self.task.abort();
        let mode = *lock(&self.inner.terminate);
        if mode == Terminate::Confirmed {
            lock(&self.inner.terminated).push(self.owner.clone());
        }
        Box::pin(async move {
            match mode {
                Terminate::Confirmed => Ok(()),
                Terminate::Fails => Err(Error::Unavailable),
                Terminate::Hangs => std::future::pending().await,
            }
        })
    }
}

impl Sandbox for FakeSandbox {
    fn admit(&self, _: &str, _: &str) -> Result<(), Error> {
        if *lock(&self.inner.refuse_admit) {
            Err(Error::Denied)
        } else {
            Ok(())
        }
    }

    fn spawn(&self, spec: SandboxSpec) -> Reply<'_, Process> {
        if *lock(&self.inner.spawn_fails) {
            return Box::pin(async { Err(Error::Unavailable) });
        }
        lock(&self.inner.specs).push(spec.clone());
        let (to_server, from_client) = duplex(1 << 16);
        let (to_client, from_server) = duplex(1 << 16);
        let (broker_effects, tool_effects) = if spec.effects && *lock(&self.inner.effects_pipe) {
            let (broker_write, tool_read) = duplex(1 << 16);
            let (tool_write, broker_read) = duplex(1 << 16);
            (
                Some(Pipe {
                    read: Box::new(broker_read) as BoxRead,
                    write: Box::new(broker_write) as BoxWrite,
                }),
                Some(Pipe {
                    read: Box::new(tool_read) as BoxRead,
                    write: Box::new(tool_write) as BoxWrite,
                }),
            )
        } else {
            (None, None)
        };
        let server = lock(&self.inner.server).clone();
        let visible_env: Vec<(String, String)> = spec
            .env
            .iter()
            .chain(spec.host_env.iter())
            .cloned()
            .collect();
        let task = tokio::spawn(serve_mcp(
            server,
            visible_env,
            tool_effects,
            Box::new(from_client),
            Box::new(to_client),
            Arc::clone(&self.inner.recorder),
        ));
        let process = Process {
            stdio: Pipe {
                read: Box::new(from_server),
                write: Box::new(to_server),
            },
            effects: broker_effects,
            handle: Box::new(FakeHandle {
                inner: Arc::clone(&self.inner),
                owner: spec.owner,
                task,
            }),
        };
        Box::pin(async move { Ok(process) })
    }

    fn effects_via_proxy(&self) -> bool {
        *lock(&self.inner.via_proxy)
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        lock(&self.inner.reaped).push(owner.to_owned());
        let ok = *lock(&self.inner.reap_ok);
        Box::pin(async move { if ok { Ok(()) } else { Err(Error::Unavailable) } })
    }
}

/// Profile-namespaced secret store isolating identical names across profiles.
#[derive(Default)]
pub struct FakeSecrets {
    shared: Arc<SecretsState>,
}

#[derive(Default)]
struct SecretsState {
    values: Mutex<HashMap<(String, String), Vec<u8>>>,
    resolved: Mutex<Vec<(String, String)>>,
}

impl FakeSecrets {
    pub fn new(entries: &[(&str, &str, &str)]) -> Self {
        let values = entries
            .iter()
            .map(|(profile, name, value)| {
                (
                    (profile.to_string(), name.to_string()),
                    value.as_bytes().to_vec(),
                )
            })
            .collect();
        Self {
            shared: Arc::new(SecretsState {
                values: Mutex::new(values),
                resolved: Mutex::default(),
            }),
        }
    }
    pub fn resolved(&self) -> Vec<(String, String)> {
        lock(&self.shared.resolved).clone()
    }
}

struct Bound {
    shared: Arc<SecretsState>,
    profile: String,
}

impl ProfileSecrets for Bound {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let key = (self.profile.clone(), name.as_str().to_owned());
        lock(&self.shared.resolved).push(key.clone());
        let found = lock(&self.shared.values).get(&key).cloned();
        Box::pin(async move { found.map(SecretValue).ok_or(Error::Missing) })
    }
}

impl SecretStore for FakeSecrets {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let bound = Bound {
            shared: Arc::clone(&self.shared),
            profile: profile.0,
        };
        Box::pin(async move { Ok(Box::new(bound) as Box<dyn ProfileSecrets>) })
    }
}
