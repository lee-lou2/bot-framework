//! OpenAI 호환 입구(공식 openai SDK) 종단 시험 조립. 실제 `claude` CLI·가짜 상류·실제 PG·합성 샌드박스 위에서 `serve`를 돌리고(`Rig::start_api`),
//! 시험이 모델이 할 일과 서버 쪽 수(작업·모델 호출·도구 실행)를 다루는 제어 창구(`control`), 모델 대본(`plan`), 공식 SDK 시험을 돌리는 도구(`sdk`)를 둔다.
//! 이 입구의 응답은 대역이 아니라 제품 게이트웨이가 만든 것이고, 모델만 가짜다.
#![allow(dead_code)]

mod control;
mod http;
mod plan;
mod sdk;
mod secrets;

use super::{Rig, cli_gateway};
use crate::process_support::{Schema, free_port, package};
use bot_cli::cli::Role;
use bot_core::port::SecretStore;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub use control::Control;
#[allow(unused_imports)]
pub use http::{Reply, call, send_and_hang_up};
#[allow(unused_imports)]
pub use plan::{ERROR_BODY, text_turn};
#[allow(unused_imports)]
pub use sdk::{Outcome, Proxy, Sdk};
pub use secrets::ApiSecrets;

/// 합성 API 키(32바이트 이상 ASCII). 값은 시험 안에서만 쓰고 어디에도 남기지 않는다.
pub struct Keys {
    pub build: String,
    pub rotated: String,
    pub ops: String,
    pub guest: String,
}

impl Keys {
    pub fn new() -> Self {
        let make = |name: &str| format!("sk-e2e-{name}-0f1e2d3c4b5a69788796a5b4c3d2e1f0");
        Self {
            build: make("build"),
            rotated: make("rotated"),
            ops: make("ops"),
            guest: make("guest"),
        }
    }

    pub fn all(&self) -> [&str; 4] {
        [&self.build, &self.rotated, &self.ops, &self.guest]
    }
}

/// 입구 하나의 모양: 평소(긴 대기·넉넉한 한도)와 빡빡한 것(짧은 대기·실행 한도 1·작은 본문 한도). 빡빡한 쪽의 대기는 3초다 — 접수(PG 트랜잭션)도 같은 단일 기한에서 `min(2초, wait/4)`를 뺀 예산을 쓰므로
/// 0.5초로는 첫 접수가 실제 PG에서 `request_outcome_unknown`(422)이 된다(관찰 기록은 보고서).
#[derive(Clone, Copy)]
pub enum Shape {
    Main,
    Tight,
}

impl Shape {
    pub fn wait_ms(self) -> u64 {
        match self {
            Shape::Main => 120_000,
            Shape::Tight => 3_000,
        }
    }
}

/// 프로필: 모델 `assistant`(에이전트 main)와 `secret-agent`(에이전트 main/secret, `run` 상한 없음 → 아무도 못 쓴다). 멤버는 http:openai/build·ops(guest는 없다).
/// 도구 `agenda.remove`는 승인이 필요한 write다(승인자 매핑이 없으니 HTTP 호출자는 실행시킬 수 없다).
pub const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  api: [run, calendar.read, calendar.delete]
  owner: [run, calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "http:openai/build", roles: [api]}
  - {identity: "http:openai/ops", roles: [api]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main}
  - {id: secret, adapter: http, endpoint: openai, model: secret-agent, agent: main/secret}
secrets: []
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/secret: [calendar.read]
  approve: [calendar.delete]
"#;

const TOOLS: &str = "version: 1\nserver: agenda\ntools:\n  - id: list\n    class: read\n    scopes: [calendar.read]\n    secrets: {}\n    egress: []\n    input: {type: object, properties: {}, additionalProperties: false}\n  - id: remove\n    class: write\n    scopes: [calendar.delete]\n    secrets: {}\n    egress: []\n    input: {type: object, properties: {id: {type: string}}, required: [id], additionalProperties: false}\n";

/// 도구 서버(MCP stdio): `list`는 빈 일정을, `remove`는 «지웠다»를 답한다. 승인 없이 `remove`가 실행되면 호출 원장에 남으니 시험이 그것을 센다.
const SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

TOOLS = [
    {"name": "list", "description": "일정 목록", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "remove", "description": "일정 삭제", "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}, "additionalProperties": False}},
]

def reply(message_id, **body):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message_id, **body}) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    message = json.loads(raw)
    if "id" not in message:
        continue
    method, identity = message.get("method"), message["id"]
    if method == "initialize":
        reply(identity, result={"protocolVersion": message["params"].get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {"listChanged": False}}, "serverInfo": {"name": "agenda", "version": "0"}})
    elif method == "ping":
        reply(identity, result={})
    elif method == "tools/list":
        reply(identity, result={"tools": TOOLS})
    elif method == "tools/call":
        name = message["params"].get("name")
        reply(identity, result={"content": [{"type": "text", "text": "[]" if name == "list" else "removed"}], "isError": False})
    else:
        reply(identity, error={"code": -32601, "message": "method not found"})
"#;

/// 시험이 쓰는 서버 한 대와 그 둘레: 제품 `serve`(Rig), 제어 창구, 키.
pub struct Fixture {
    pub rig: Rig,
    pub port: u16,
    pub keys: Keys,
    pub control: Control,
    pub secrets: Arc<ApiSecrets>,
    pub shape: Shape,
}

impl Rig {
    /// `start_with`와 같되 호스트 `servers` 같은 최상위 절과 저장소 설정까지 한 번에 받는다. 활성 판본이 게시될 때까지 기다린다.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_api(
        schema: Schema,
        claude: PathBuf,
        profile_yaml: &str,
        extra: &[(&str, &str)],
        gateway: &str,
        host_extra: &str,
        secrets: Arc<dyn SecretStore>,
        store_config: Option<bot_store::Config>,
    ) -> Rig {
        let mut rig = Self::launch_inner(
            schema,
            claude,
            profile_yaml,
            extra,
            gateway,
            &[Role::Gateway, Role::Worker],
            secrets,
            host_extra,
            None,
            None,
            store_config,
        )
        .await;
        rig.until(
            "serve가 활성 판본을 게시하지 않았어요",
            30,
            |rig| !rig.schema.active().is_empty(),
        )
        .await;
        rig
    }
}

impl Rig {
    /// 운영자가 호스트 설정의 `gateway` 절(예: 키 슬롯)을 바꿔 같은 프로필로 `--activate` 다시 배포한다: 호스트 파일을 다시 쓰고 새 serve가 새 판본을 게시할 때까지 기다린다.
    pub async fn reactivate_host(&mut self, gateway: &str, profile_yaml: &str, roles: &[Role]) {
        self.gateway = gateway.to_owned();
        let host = self.dir.path().join("host.json");
        std::fs::write(
            host,
            super::host_config(gateway, super::ENDPOINT, &self.host_extra),
        )
        .unwrap();
        self.reactivate(profile_yaml, roles).await;
    }
}

/// 호스트 `gateway` 절(CLI 계정 + OpenAI 입구 하나 + 모양에 따른 본문 한도). 키 슬롯 이름은 호스트 쪽 이름표이고 값은 합성 비밀 보관소에만 있다.
pub fn gateway_for(shape: Shape, port: u16) -> String {
    gateway_with(shape, port, &["build", "rotated", "ops", "guest"])
}

/// `gateway_for`와 같되 키 슬롯을 고른다(`build`·`rotated`는 주체 build, `ops`는 주체 ops, `guest`는 주체 guest) — 운영자가 슬롯을 빼고 다시 배포하는 시험에 쓴다.
pub fn gateway_with(shape: Shape, port: u16, slots: &[&str]) -> String {
    let keys: Vec<String> = slots
        .iter()
        .map(|slot| {
            let subject = if *slot == "rotated" { "build" } else { slot };
            format!(r#""{slot}": {{"subject": "{subject}", "secret": "{slot}-api-key"}}"#)
        })
        .collect();
    let limits = match shape {
        Shape::Main => String::new(),
        Shape::Tight => r#", "limits": {"body_bytes": 4096}"#.to_owned(),
    };
    let active = match shape {
        Shape::Main => 32,
        Shape::Tight => 1,
    };
    format!(
        r#"{cli}, "http": {{"listen": "127.0.0.1:{port}", "endpoints": {{"openai": {{"kind": "openai", "wait_ms": {wait}, "max_active_runs": {active},
            "keys": {keys}}}}}}}{limits}"#,
        cli = cli_gateway(),
        wait = shape.wait_ms(),
        keys = format_args!("{{{}}}", keys.join(", ")),
    )
}

fn tool_package(dir: &Path) -> PathBuf {
    package(dir, SERVER)
}

impl Fixture {
    /// 서버를 띄운다. 환경이 없어 시험을 돌릴 수 없으면 `None`(건너뜀). 필수 모드(`BOT_OPENAI_CANARY=required`·`BOT_CLAUDE_CANARY`·`BOT_SRT_CANARY`·`BOT_TEST_REQUIRE_DB`)에서는 실패한다.
    pub async fn start(
        shape: Shape,
        store_config: Option<bot_store::Config>,
    ) -> Option<(Self, Sdk)> {
        Self::start_with_profile(shape, store_config, PROFILE).await
    }

    /// `start`와 같되 프로필(`personal/profile.yaml`)을 고른다.
    pub async fn start_with_profile(
        shape: Shape,
        store_config: Option<bot_store::Config>,
        profile: &str,
    ) -> Option<(Self, Sdk)> {
        let Some(schema) = Schema::create() else {
            eprintln!("skipped: BOT_TEST_DATABASE_URL이 없어 건너뜀");
            return None;
        };
        let claude = super::claude_bin()?;
        if !crate::process_support::have_srt() {
            return None;
        }
        let sdk = Sdk::locate()?;
        let keys = Keys::new();
        let secrets = ApiSecrets::new(&[
            ("model-key", "provider-key-for-personal"),
            ("build-api-key", &keys.build),
            ("rotated-api-key", &keys.rotated),
            ("ops-api-key", &keys.ops),
            ("guest-api-key", &keys.guest),
        ]);
        let port = free_port();
        let dir = tempfile::Builder::new()
            .prefix("o")
            .tempdir_in("/tmp")
            .unwrap();
        let pkg = tool_package(dir.path());
        let host_extra = format!(
            r#", "servers": {{"agenda": {{"root": "{}", "command": ["bin/server", "--stdio"]}}}}"#,
            pkg.display()
        );
        let rig = Rig::start_api(
            schema,
            claude,
            profile,
            &[
                ("agents/main/agents/secret/AGENT.md", "비공개 에이전트."),
                ("agents/main/skills/agenda/SKILL.md", "일정을 지운다."),
                ("agents/main/skills/agenda/tools.yaml", TOOLS),
            ],
            &gateway_for(shape, port),
            &host_extra,
            secrets.clone(),
            store_config,
        )
        .await;
        std::mem::forget(dir); // 도구 패키지는 서버가 도는 동안 필요하다(/tmp 밑이고 시험 프로세스가 끝나면 OS가 치운다)
        let control = Control::start(&rig).await;
        Some((
            Self {
                rig,
                port,
                keys,
                control,
                secrets,
                shape,
            },
            sdk,
        ))
    }

    /// 입구 주소(루트 `/v1`의 바로 앞까지)
    pub fn origin(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// 서버가 듣는 PG에서 키 값이 한 번이라도 평문으로 저장됐는지(스키마 전 테이블의 모든 행을 글로 보아 찾는다). 찾은 (테이블, 키 이름)들.
    pub fn stored_secrets(&self) -> Vec<(String, &'static str)> {
        let mut found = Vec::new();
        for (label, key) in [
            ("build", &self.keys.build),
            ("rotated", &self.keys.rotated),
            ("ops", &self.keys.ops),
            ("guest", &self.keys.guest),
        ] {
            found.extend(self.tables_holding(key).into_iter().map(|t| (t, label)));
        }
        found
    }

    /// 스키마의 모든 표를 행마다 글로 보아 이 글이 든 표들(없으면 비었다)
    pub fn tables_holding(&self, needle: &str) -> Vec<String> {
        let tables = self.rig.query(
            "select string_agg(table_name, ',') from information_schema.tables where table_schema = '{s}'",
        );
        tables
            .split(',')
            .filter(|t| !t.is_empty())
            .filter(|table| {
                self.rig.query(&format!(
                    "select count(*) from {{s}}.{table} t where t::text like '%{needle}%'"
                )) != "0"
            })
            .map(str::to_owned)
            .collect()
    }

    /// 입구에 직접 보내는 요청(SDK 없이): 응답을 끝까지 읽는다.
    pub async fn request(
        &self,
        method: &'static str,
        path: &'static str,
        key: &str,
        body: Option<&'static str>,
        extra: Vec<(&'static str, String)>,
    ) -> Reply {
        let (origin, key) = (self.origin(), key.to_owned());
        tokio::task::spawn_blocking(move || {
            let extra: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (*k, v.as_str())).collect();
            call(&origin, method, path, &key, body, &extra)
        })
        .await
        .unwrap()
    }
}
