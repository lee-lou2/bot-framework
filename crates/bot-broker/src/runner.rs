//! 도구 실행기: 승인까지 끝난 호출 하나를 별도 샌드박스의 MCP 서버로 실행한다(`ToolRunner` 구현).
//!
//! 호출마다 새로 만든다: 선언한 자격만 이 프로필의 이름공간에서 풀어 환경에 넣고(형제 도구의 자격은 없다),
//! 선언한 호스트로만 가는 egress 프록시를 열고, 샌드박스를 띄워 MCP를 협상하고, 입력 스키마를 선언과 대조한 뒤
//! 도구 하나만 `_meta` 호출 ID와 함께 부른다. 결과는 형식·크기·자격 노출을 검사하고 프로세스를 끝낸다.
//!
//! 실패 분류가 복구를 정한다: `tools/call`을 보내기 전 실패는 `NotSent`(미실행 입증), 보낸 뒤 결과를 모르면
//! `Unknown`, 실행은 끝났지만 결과를 막았으면 `Blocked`다.

use crate::{
    canon::canonicalize,
    effects::EffectChannel,
    egress::EgressPolicy,
    guard::blocks_output,
    json::parse,
    mcp::{self, McpError, Session, remote::Remote, wire::RemoteWire},
    ports::{Ran, ToolJob, ToolRunner},
    proxy::{Connector, LOCAL_HOST, LocalService, Proxy, ProxyConfig, Resolver},
    resolve::HostPort,
    sandbox::{Limits, Process, ProxyEndpoint, Sandbox, SandboxSpec},
    sys::Entropy,
    text::{hex, json_quote},
};
use bot_core::{
    command::JsonPayload,
    id::{ProfileId, SecretName},
    port::{EffectJournal, Error, Reply, SecretStore},
    profile::Tool,
    state::ConversationKind,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, time::timeout};

/// 호스트(운영자)가 등록한 것: 설치한 서버 판본과 프로필별 egress 상한.
#[derive(Clone, Debug, Default)]
pub struct HostRegistry {
    /// 서버 별칭 → 고정한 실행 파일·이미지 판본.
    pub servers: BTreeMap<String, String>,
    /// 프로필 → 운영자가 정한 egress 상한. 없으면 도구가 선언한 호스트만 허용한다.
    pub egress_caps: BTreeMap<ProfileId, Vec<HostPort>>,
    /// 원격(Streamable HTTP) 서버 별칭 → 접속 대상. 이 별칭은 `servers`에도 있다(판본은 `Remote::revision`). 원격 서버는 샌드박스를 거치지 않고 브로커가 직접 연결한다.
    pub remote: BTreeMap<String, Remote>,
}

#[derive(Clone, Copy, Debug)]
pub struct RunnerConfig {
    /// `initialize`와 `tools/list` 각각의 제한.
    pub handshake_timeout: Duration,
    pub call_timeout: Duration,
    pub terminate_timeout: Duration,
    pub max_result_bytes: usize,
    /// 운영자가 사설망 접속을 명시적으로 열었을 때만 true.
    pub allow_private_egress: bool,
    /// 샌드박스 전체 수명 상한.
    pub wall: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(60),
            terminate_timeout: Duration::from_secs(10),
            max_result_bytes: 256 * 1024,
            allow_private_egress: false,
            wall: Duration::from_secs(300),
        }
    }
}

pub struct Deps {
    pub sandbox: Arc<dyn Sandbox>,
    pub secrets: Arc<dyn SecretStore>,
    pub effects: Arc<dyn EffectJournal>,
    pub resolver: Arc<dyn Resolver>,
    pub connector: Arc<dyn Connector>,
    pub entropy: Arc<dyn Entropy>,
}

pub struct McpRunner {
    deps: Deps,
    host: HostRegistry,
    cfg: RunnerConfig,
    /// 원격 서버로 나가는 전송. 없으면 원격 서버로 가는 호출은 실행하지 않는다.
    wire: Option<Arc<dyn RemoteWire>>,
}

mod remote;

fn kind_name(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

/// `tools/call`의 `params._meta`. 호스트가 넣으며 모델이 지정할 수 없다.
fn meta(job: &ToolJob) -> String {
    let mut out = format!("{{\"bot/call-id\":{}", json_quote(&job.call.0));
    if let Some(to) = &job.reply_to {
        let thread = to
            .thread
            .as_deref()
            .map_or_else(|| "null".to_owned(), json_quote);
        out.push_str(&format!(
            ",\"bot/destination\":{{\"id\":{},\"kind\":{},\"thread\":{thread}}}",
            json_quote(&to.id),
            json_quote(kind_name(&to.kind)),
        ));
    }
    // 외부 멱등키는 [profile-id, run-id, call-id](다단계면 step도)다. 호출 ID는 run 안의 위치라 서로 다른 run의 첫 호출이
    // 같을 수 있으므로 셋을 함께 넣는다. generation·claim은 인계해도 같은 논리 호출이어야 하므로 키에 쓰지 않는다.
    out.push_str(&format!(
        ",\"bot/generation\":{},\"bot/principal\":{},\"bot/profile-id\":{},\"bot/run-id\":{},\"bot/trigger\":{}}}",
        job.attempt.generation.0,
        json_quote(&job.principal.0),
        json_quote(&job.attempt.profile.0),
        json_quote(&job.attempt.run.0),
        json_quote(job.origin.as_str())
    ));
    out
}

/// 환경 변수 값 하나의 최대 길이(바이트). 프로필 검증이 더 좁게 막지만 시작 직전에도 한도를 둔다.
const MAX_ENV_VALUE: usize = 8 << 10;

/// 단계 원장 콜백을 프록시의 전용 주소로 받는 처리기.
struct EffectLocal(EffectChannel);

impl LocalService for EffectLocal {
    fn handle(&self, frame: Vec<u8>) -> Reply<'_, Vec<u8>> {
        Box::pin(async move { Ok(self.0.handle(&frame).await.into_bytes()) })
    }
}

/// 전용 주소로 단계 원장을 받는 샌드박스에 알려 줄 주소.
const EFFECT_URL: &str = "http://bot-effects.invalid/effect";

/// 서버가 내세우는 도구 정의(`entry`)가 선언과 맞는가. 정의 해시가 고정된 도구(`tools.lock.yaml`)는 서버의 이름·설명·입력 스키마가 해시와 같아야 한다
/// (설명만 바뀐 것도 막는다: 도구 오염). 다르거나 사라졌으면 호출하기 전에 `Refused`다. 그 밖에는 입력 스키마가 선언과 같아야 한다(아니면 `NotSent`).
fn check_definition(tool: &Tool, entry: Option<&Value>, declared: &str) -> Result<(), Ran> {
    if let Some(pin) = &tool.definition {
        match entry {
            None => return Err(Ran::Refused("tool_missing")),
            Some(found) if bot_core::mcp::entry_sha256(found).as_deref() != Some(pin.as_str()) => {
                return Err(Ran::Refused("tool_definition_changed"));
            }
            Some(_) => {}
        }
    }
    let schema = entry
        .and_then(|found| found.get("inputSchema"))
        .and_then(canonicalize);
    if schema.as_deref() == Some(declared) {
        Ok(())
    } else {
        Err(Ran::NotSent)
    }
}

/// 구조화 결과의 모든 키와 디코딩한 문자열 값을 검사한다(직렬화한 글만 보면 이스케이프가 필요한 문자가 든 자격을 놓친다).
fn structured_blocked(value: &Value, credentials: &[Vec<u8>]) -> bool {
    match value {
        Value::String(text) => blocks_output(text, credentials),
        // 숫자 토큰도 글이다(숫자로만 된 자격이 숫자로 오는 경우)
        Value::Number(number) => blocks_output(&number.to_string(), credentials),
        Value::Array(items) => items
            .iter()
            .any(|item| structured_blocked(item, credentials)),
        Value::Object(map) => map.iter().any(|(name, item)| {
            blocks_output(name, credentials) || structured_blocked(item, credentials)
        }),
        _ => false,
    }
}

impl McpRunner {
    pub fn new(deps: Deps, host: HostRegistry, cfg: RunnerConfig) -> Self {
        Self {
            deps,
            host,
            cfg,
            wire: None,
        }
    }

    /// 원격 서버로 나가는 전송(조립하는 곳이 이름 풀이 검사·TLS를 맡는다, `mcp::wire`).
    pub fn with_wire(mut self, wire: Arc<dyn RemoteWire>) -> Self {
        self.wire = Some(wire);
        self
    }

    async fn execute(&self, job: ToolJob) -> Ran {
        let started = tokio::time::Instant::now();
        let tool = &job.tool.tool;
        // 승인이 묶은 설치 판본이 지금 등록부와 같아야 한다.
        let Some(image) = self.host.servers.get(&tool.server).cloned() else {
            return Ran::NotSent;
        };
        if tool
            .revision
            .0
            .rsplit_once('@')
            .map(|(_, approved)| approved)
            != Some(image.as_str())
        {
            return Ran::NotSent;
        }
        // 원격 서버는 샌드박스 없이 브로커가 직접 연결한다
        if let Some(remote) = self.host.remote.get(&tool.server) {
            return self.execute_remote(&job, remote).await;
        }
        // 샌드박스가 이 서버를 실행할 수 있어야 한다(상태 볼륨을 요구하는데 지원하지 않으면 자격을 풀기 전에 시작하지 않는다).
        if self.deps.sandbox.admit(&tool.server, &image).is_err() {
            return Ran::NotSent;
        }
        // 선언한 자격만, 이 프로필의 이름공간에서 푼다.
        let (mut env, mut credentials) = (Vec::new(), Vec::new());
        if !tool.secrets.is_empty() {
            let Ok(store) = self
                .deps
                .secrets
                .for_profile(job.attempt.profile.clone())
                .await
            else {
                return Ran::NotSent;
            };
            for (var, name) in &tool.secrets {
                // 호스트 전용 자격(비공개 접수의 지문 키)은 도구에 주지 않는다.
                if crate::private::reserved_secret(name) {
                    return Ran::NotSent;
                }
                let Ok(value) = store.resolve(name.clone()).await else {
                    return Ran::NotSent;
                };
                let Ok(text) = String::from_utf8(value.0.clone()) else {
                    return Ran::NotSent;
                };
                if text.contains('\0') {
                    return Ran::NotSent;
                }
                credentials.push(value.0);
                env.push((var.clone(), text));
            }
        }
        // 도구가 고른 비밀 아닌 환경 변수: 이름이 로더·셸·프록시·호스트 예약이면 안 되고, 선언한 비밀의 변수 이름과 겹치면 안 된다
        // (스키마 검증을 이미 거쳤더라도 시작 직전에 한 번 더 본다). 값은 결과 검사용 자격에 넣지 않는다(비밀이 아니다).
        // 값은 도구가 선언한 이름에 대해서만, 선언한 이름마다 정확히 하나씩 온다(호스트가 고른 값이 선언과 어긋나면 시작하지 않는다).
        let given: BTreeSet<&String> = job.env.iter().map(|(name, _)| name).collect();
        if given.len() != job.env.len()
            || given.len() != tool.env.len()
            || !tool.env.iter().all(|name| given.contains(name))
        {
            return Ran::NotSent;
        }
        for (name, value) in &job.env {
            let clash = env.iter().any(|(taken, _)| taken == name);
            if bot_core::profile::forbidden_env_name(name)
                || clash
                || value.contains('\0')
                || value.len() > MAX_ENV_VALUE
            {
                return Ran::NotSent;
            }
            env.push((name.clone(), value.clone()));
        }
        // 샌드박스가 FD를 줄 수 없으면 단계 원장은 프록시의 전용 주소로 받는다.
        let via_proxy = tool.effects && self.deps.sandbox.effects_via_proxy();
        // 선언한 호스트 ∩ 운영자 상한으로만 나가는 프록시. 허용할 곳이 없으면 네트워크는 0이다.
        let cap = self
            .host
            .egress_caps
            .get(&job.attempt.profile)
            .map(Vec::as_slice);
        let policy = EgressPolicy::new(&job.tool.egress, cap);
        let (mut proxy, mut endpoint, mut host_env) = (None, None, Vec::new());
        let mut hosts: Vec<String> = policy
            .allowed()
            .iter()
            .map(|h| format!("{}:{}", h.host, h.port))
            .collect();
        let mut effect_channel = None;
        if !policy.is_empty() || via_proxy {
            let mut raw = [0u8; 16];
            if !self.deps.entropy.fill(&mut raw) {
                return Ran::NotSent;
            }
            let password = hex(&raw);
            let mut proxy_cfg = ProxyConfig::new(policy, "bot".into(), password.clone());
            proxy_cfg.allow_private = self.cfg.allow_private_egress;
            // 프록시 암호도 이 호출이 쓰는 자격이다(결과와 단계 보고 검사에 함께 쓴다)
            credentials.push(password.clone().into_bytes());
            if via_proxy {
                let chan = EffectChannel::new(
                    Arc::clone(&self.deps.effects),
                    job.attempt.clone(),
                    job.call.clone(),
                    job.tool.id.clone(),
                )
                .with_credentials(credentials.clone());
                proxy_cfg.local = Some(Arc::new(EffectLocal(chan)));
                hosts.push(format!("{LOCAL_HOST}:80"));
                host_env.push(("BOT_EFFECT_URL".to_owned(), EFFECT_URL.to_owned()));
                effect_channel = Some(());
            }
            let started_proxy = Proxy::start(
                proxy_cfg,
                Arc::clone(&self.deps.resolver),
                Arc::clone(&self.deps.connector),
            )
            .await;
            let Ok(started_proxy) = started_proxy else {
                return Ran::NotSent;
            };
            let url = format!("http://bot:{password}@{}", started_proxy.addr());
            for name in [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                env.push((name.to_owned(), url.clone()));
            }
            endpoint = Some(ProxyEndpoint {
                addr: started_proxy.addr(),
                url,
            });
            proxy = Some(started_proxy);
        }
        let spec = SandboxSpec {
            owner: job.owner.clone(),
            server: tool.server.clone(),
            image,
            env,
            host_env,
            egress: endpoint.take(),
            egress_hosts: hosts,
            effects: tool.effects && effect_channel.is_none(),
            limits: Limits {
                wall: self.cfg.wall,
            },
        };
        // 시작부터 쓰기·응답까지 절대 기한 하나로 묶는다. 어느 단계에서 막혀도(쓰기가 막혀 타이머에 못 닿는 경우 포함) 기한이 끝나면
        // 프로세스 그룹과 프록시를 닫고 "호출을 보냈는가"로 정산한다.
        let ran = match timeout(self.cfg.wall, self.deps.sandbox.spawn(spec)).await {
            Ok(Ok(process)) => {
                let Process {
                    stdio,
                    effects,
                    handle,
                } = process;
                let sent = AtomicBool::new(false);
                let remaining = self.cfg.wall.saturating_sub(started.elapsed());
                let ran = match timeout(
                    remaining,
                    self.drive(&job, stdio, effects, &credentials, &sent),
                )
                .await
                {
                    Ok(ran) => ran,
                    Err(_) if sent.load(Ordering::SeqCst) => Ran::Unknown("wall_timeout"),
                    Err(_) => Ran::NotSent,
                };
                // 결과를 얻었으면 프로세스 그룹 전체를 끝내고 확인한다.
                let confirmed = matches!(
                    timeout(self.cfg.terminate_timeout, handle.terminate()).await,
                    Ok(Ok(()))
                );
                match ran {
                    // 띄운 프로세스가 끝났는지 모르면 "보내지 않았다"를 입증할 수 없다(남은 프로세스가 보낼 수 있다).
                    Ran::NotSent | Ran::Rejected if !confirmed => {
                        Ran::Unknown("terminate_unconfirmed")
                    }
                    other => other,
                }
            }
            // 프로세스를 띄우지 못했다.
            Ok(Err(_)) => Ran::NotSent,
            // 띄우는 도중 기한이 지났다: 만들어진 프로세스가 남아 있을 수 있으니 소유자 기준으로 끝내고 확인한다.
            Err(_) => match timeout(
                self.cfg.terminate_timeout,
                self.deps.sandbox.reap(&job.owner),
            )
            .await
            {
                Ok(Ok(())) => Ran::NotSent,
                _ => Ran::Unknown("terminate_unconfirmed"),
            },
        };
        if let Some(proxy) = proxy.take() {
            proxy.stop().await;
        }
        ran
    }

    async fn drive(
        &self,
        job: &ToolJob,
        stdio: crate::sandbox::Pipe,
        effects: Option<crate::sandbox::Pipe>,
        credentials: &[Vec<u8>],
        sent: &AtomicBool,
    ) -> Ran {
        let mut session = Session::new(stdio);
        let limit = self.cfg.handshake_timeout;
        if session.initialize(limit).await.is_err() {
            return Ran::NotSent;
        }
        // 일치하는 도구 하나만 부른다: 서버가 내세우는 정의가 선언과 같아야 한다.
        let Some(declared) = parse(&job.tool.tool.input.0).and_then(|v| canonicalize(&v)) else {
            return Ran::NotSent;
        };
        let entry = match session.find_tool(&job.tool.name, limit).await {
            Ok(entry) => entry,
            Err(_) => return Ran::NotSent,
        };
        if let Err(ran) = check_definition(&job.tool.tool, entry.as_ref(), &declared) {
            return ran;
        }
        // 파이프 채널은 `effects: true`이고 샌드박스가 FD를 줄 수 있을 때만 있다(프록시 경유 샌드박스는 파이프가 없다).
        let channel = match (job.tool.tool.effects, effects) {
            (true, Some(pipe)) => {
                let (stop, signal) = watch::channel(false);
                let chan = EffectChannel::new(
                    Arc::clone(&self.deps.effects),
                    job.attempt.clone(),
                    job.call.clone(),
                    job.tool.id.clone(),
                )
                .with_credentials(credentials.to_vec());
                Some((
                    stop,
                    tokio::spawn(chan.serve(pipe.read, pipe.write, signal)),
                ))
            }
            (true, None) if !self.deps.sandbox.effects_via_proxy() => return Ran::NotSent,
            _ => None,
        };
        sent.store(true, Ordering::SeqCst);
        let result = match session
            .send_call(&job.tool.name, &job.args, &meta(job))
            .await
        {
            Ok(id) => session.await_result(id, self.cfg.call_timeout).await,
            Err(_) => Err(McpError::Io),
        };
        if let Some((stop, task)) = channel {
            // 처리 중이던 콜백 프레임은 마치고 닫는다.
            let _ = stop.send(true);
            let _ = timeout(Duration::from_secs(5), task).await;
        }
        match result {
            Ok(value) => self.vet(&value, credentials),
            // 잘못된 요청·없는 메서드·잘못된 인자는 서버가 실행 전에 거절한 것이다. 그 밖의 서버 오류는 효과를 알 수 없다.
            Err(McpError::Server(-32602..=-32600)) => Ran::Rejected,
            Err(McpError::Server(_)) => Ran::Unknown("server_error"),
            Err(McpError::Timeout) => Ran::Unknown("timeout"),
            Err(McpError::Closed) => Ran::Unknown("closed"),
            Err(McpError::Io) => Ran::Unknown("io"),
            Err(_) => Ran::Unknown("protocol"),
        }
    }

    /// 결과의 형식·크기·알려진 자격 노출을 검사한 뒤 고정 형식으로 다시 만든다.
    fn vet(&self, value: &Value, credentials: &[Vec<u8>]) -> Ran {
        let output = match mcp::parse_result(value) {
            Ok(output) => output,
            Err(code) => return Ran::Blocked(code),
        };
        let mut size = 0usize;
        for text in &output.texts {
            size += text.len();
            if blocks_output(text, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        if let Some(structured) = &output.structured {
            let Some(text) = canonicalize(structured) else {
                return Ran::Blocked("result_shape");
            };
            size += text.len();
            if structured_blocked(structured, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        // 후속 확인 요청도 모델에게 가는 글을 담을 수 있으니 같은 검사(크기·자격 노출·정규화)를 받는다.
        if let Some(wake) = &output.wake {
            let Some(text) = canonicalize(wake) else {
                return Ran::Blocked("result_shape");
            };
            size += text.len();
            if structured_blocked(wake, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        if size > self.cfg.max_result_bytes {
            return Ran::Blocked("result_size");
        }
        match mcp::payload(&output) {
            Some(payload) if payload.len() <= self.cfg.max_result_bytes => {
                Ran::Done(JsonPayload(payload))
            }
            Some(_) => Ran::Blocked("result_size"),
            None => Ran::Blocked("result_shape"),
        }
    }
}

impl ToolRunner for McpRunner {
    fn installed(&self, server: &str) -> Option<String> {
        self.host.servers.get(server).cloned()
    }

    fn admit(&self, server: &str) -> Result<(), Error> {
        let image = self.host.servers.get(server).ok_or(Error::Denied)?;
        if self.host.remote.contains_key(server) {
            return if self.wire.is_some() {
                Ok(())
            } else {
                Err(Error::Denied)
            };
        }
        self.deps.sandbox.admit(server, image)
    }

    fn server_secrets(&self, server: &str) -> Vec<SecretName> {
        self.host
            .remote
            .get(server)
            .and_then(|remote| remote.bearer.clone())
            .into_iter()
            .collect()
    }

    /// 원격 서버는 연결을 닫아도 원격 실행이 계속될 수 있다: `reap` 성공이 끝남의 증명이 아니다.
    fn reap_proves_end(&self, server: &str) -> bool {
        !self.host.remote.contains_key(server)
    }

    /// 이 프로필의 보관소에 선언한 자격 이름이 모두 있는가(없으면 `Missing`). 값은 읽어도 곧바로 버리고 쓰거나 남기지 않는다.
    fn admit_secrets(&self, profile: &ProfileId, names: &[SecretName]) -> Reply<'_, ()> {
        let (profile, names) = (profile.clone(), names.to_vec());
        Box::pin(async move {
            if names.is_empty() {
                return Ok(());
            }
            let store = self.deps.secrets.for_profile(profile).await?;
            for name in names {
                drop(store.resolve(name).await?);
            }
            Ok(())
        })
    }

    fn run(&self, job: ToolJob) -> Reply<'_, Ran> {
        Box::pin(async move { Ok(self.execute(job).await) })
    }

    fn reap(&self, owner: String) -> Reply<'_, ()> {
        Box::pin(async move { self.deps.sandbox.reap(&owner).await })
    }
}

#[cfg(test)]
mod tests;
