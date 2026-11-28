//! Sandboxed local stdio MCP tool listing for `bot mcp sync` using `SrtSandbox`.

use crate::{host::HostFile, serve::runner::local_sandbox, settings::Settings};
use bot_broker::{
    mcp::{McpError, Session},
    sandbox::{Limits as SandboxLimits, Sandbox, SandboxSpec},
};
use bot_profile::{
    Limits,
    locate::{Located, parse_tools_yaml, read_env_yaml},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const WALL_LIMIT: Duration = Duration::from_secs(30);

/// Collects non-secret `env.yaml` values declared along the skill's profile/agent lineage,
/// filtered strictly to env variable names referenced in `tools.yaml`.
fn declared_non_secret_env(located: &Located) -> Vec<(String, String)> {
    let limits = Limits::default();
    let Ok(doc) = parse_tools_yaml(
        &located.name,
        "tools.yaml",
        &located.tools_yaml,
        None,
        false,
        &limits,
    ) else {
        return Vec::new();
    };
    let wanted: BTreeSet<String> = doc
        .value
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("env").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    let mut dirs = vec![located.profile_dir.clone()];
    // Walk any `agents/<name>` prefixes above the skill directory so agent-level env.yaml applies too.
    let mut cur = located.profile_dir.clone();
    let parts: Vec<&str> = located.skill.split('/').collect();
    let mut i = 0;
    while i + 1 < parts.len() && parts[i] == "agents" {
        cur = cur.join("agents").join(parts[i + 1]);
        dirs.push(cur.clone());
        i += 2;
    }
    for dir in dirs {
        merged.extend(read_env_yaml(&dir.join("env.yaml"), &limits));
    }
    merged
        .into_iter()
        .filter(|(k, _)| wanted.contains(k))
        .collect()
}

/// Lists tools from a single local stdio MCP server inside `SrtSandbox` with zero egress and zero secrets.
pub async fn list_stdio_tools(
    host: &HostFile,
    settings: &Settings,
    located: &Located,
    server: &str,
    uid: u32,
    timeout: Duration,
) -> Result<Vec<Value>, String> {
    let spec = host
        .servers
        .get(server)
        .ok_or_else(|| format!("server is not registered in host config: {server}"))?;
    // Install and sandbox only the single target server needed for this sync.
    let mut single = crate::host::parse(r#"{"version": 1}"#).map_err(|e| e.to_string())?;
    single.sandbox = host.sandbox.clone();
    single.servers.insert(server.to_owned(), spec.clone());
    let (installs, sandbox) = local_sandbox(&single, settings, uid).await?;
    let installed = installs
        .get(server)
        .ok_or_else(|| format!("server `{server}` failed to install"))?;
    let owner = format!("mcp-sync-{}", bot_core::random::random_hex(8));
    let env = declared_non_secret_env(located);
    let process = sandbox
        .spawn(SandboxSpec {
            owner: owner.clone(),
            server: server.to_owned(),
            image: installed.revision.0.clone(),
            env,
            host_env: Vec::new(),
            egress: None,
            egress_hosts: Vec::new(),
            effects: false,
            limits: SandboxLimits { wall: WALL_LIMIT },
        })
        .await
        .map_err(|_| {
            format!("failed to start sandboxed stdio server `{server}` (check sandbox.srt config)")
        })?;
    let mut session = Session::new(process.stdio);
    let handle = process.handle;
    let listed = async {
        session.initialize(timeout).await?;
        session.tools(timeout).await
    }
    .await;
    drop(session);
    let _ = handle.terminate().await;
    let _ = sandbox.reap(&owner).await;
    // Keep `installs` alive until the sandboxed child is reaped so its read-only dir lock holds.
    drop(installs);
    listed.map_err(|err| describe_stdio(server, &err))
}

fn describe_stdio(server: &str, err: &McpError) -> String {
    match err {
        McpError::Closed => format!(
            "stdio server `{server}` exited before listing tools (`server_exit_before_list`) — check server startup requirements (note: `bot mcp sync` passes no secrets and no network egress)"
        ),
        McpError::Timeout => format!("stdio server `{server}` timed out during tools/list"),
        McpError::Version => {
            format!("stdio server `{server}` does not support a compatible MCP protocol version")
        }
        other => format!(
            "failed to list tools from stdio server `{server}`: {}",
            super::describe(other)
        ),
    }
}
