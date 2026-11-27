//! OpenAI-compatible HTTP gateway e2e harness: runs real `serve` over fake upstream, real PG, and synthetic sandbox with control plane and official SDK runners.
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

/// Synthetic API keys (>=32 ASCII bytes) kept strictly in memory for test runs.
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

/// Endpoint sizing preset: `Tight` uses a 3s wait because PG intake takes `min(2s, wait/4)` out of the
/// single request deadline (0.5s causes real PG intake to hit `request_outcome_unknown` / 422).
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

/// Exposes `assistant` (`main`) and `secret-agent` (`main/secret`, lacking `run` scope); `agenda.remove` requires approval.
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

const SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

TOOLS = [
    {"name": "list", "description": "List agenda", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "remove", "description": "Remove agenda item", "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}, "additionalProperties": False}},
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

pub struct Fixture {
    pub rig: Rig,
    pub port: u16,
    pub keys: Keys,
    pub control: Control,
    pub secrets: Arc<ApiSecrets>,
    pub shape: Shape,
}

impl Rig {
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
        rig.until("serve did not publish an active revision", 30, |rig| {
            !rig.schema.active().is_empty()
        })
        .await;
        rig
    }
}

impl Rig {
    /// Rewrites `host.json` with a new `gateway` section and re-activates the profile.
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

pub fn gateway_for(shape: Shape, port: u16) -> String {
    gateway_with(shape, port, &["build", "rotated", "ops", "guest"])
}

/// Builds a `gateway` JSON fragment for the selected key slots (`rotated` maps to subject `build`).
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
    pub async fn start(
        shape: Shape,
        store_config: Option<bot_store::Config>,
    ) -> Option<(Self, Sdk)> {
        Self::start_with_profile(shape, store_config, PROFILE).await
    }

    pub async fn start_with_profile(
        shape: Shape,
        store_config: Option<bot_store::Config>,
        profile: &str,
    ) -> Option<(Self, Sdk)> {
        let Some(schema) = Schema::create() else {
            eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
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
                ("agents/main/agents/secret/AGENT.md", "Private agent."),
                (
                    "agents/main/skills/agenda/SKILL.md",
                    "Removes agenda items.",
                ),
                ("agents/main/skills/agenda/tools.yaml", TOOLS),
            ],
            &gateway_for(shape, port),
            &host_extra,
            secrets.clone(),
            store_config,
        )
        .await;
        std::mem::forget(dir); // Tool package must stay on disk while the server runs.
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

    pub fn origin(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Scans all tables in the test schema for any plaintext API key leak.
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
