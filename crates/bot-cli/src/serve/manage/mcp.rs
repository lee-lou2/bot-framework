//! `mcp.add` entry point and remote Streamable HTTP MCP registration, tool sync, and live activation.

use super::{
    LiveManager,
    yaml_edit::{
        DiskTxn, host_add_server, profile_add_scopes_and_secrets, render_skill_md, render_yaml,
        skill_rel_dir,
    },
};
use crate::{mcp_sync, serve::remote::ReqwestWire};
use bot_broker::{
    guard::blocks_output,
    mcp::{http::Http, remote::Remote},
    proxy::SystemResolver,
};
use bot_core::{
    command::JsonPayload,
    id::{AgentId, ProfileId, SecretName, is_name},
    manage::{ManageCall, ManageError, ManageOutcome},
    port::SecretStore,
    profile::Snapshot,
};
use bot_profile::{Limits, lock};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const SYNC_LIMIT: Duration = Duration::from_secs(15);

pub(super) fn reject(code: &'static str, message: impl Into<String>) -> ManageError {
    ManageError::Rejected {
        code,
        message: message.into(),
    }
}

pub(super) fn sanitize_tool_id(raw: &str, used: &mut BTreeSet<String>) -> String {
    let mut base = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            base.push(lower);
            prev_dash = false;
        } else if !prev_dash && !base.is_empty() {
            base.push('-');
            prev_dash = true;
        }
    }
    let trimmed = base.trim_matches('-');
    let mut candidate = if trimmed.is_empty() || !trimmed.as_bytes()[0].is_ascii_lowercase() {
        format!("t-{trimmed}")
    } else {
        trimmed.to_owned()
    };
    candidate.truncate(58);
    candidate = candidate.trim_end_matches('-').to_owned();
    if !is_name(&candidate) {
        candidate = "tool".into();
    }
    if used.insert(candidate.clone()) {
        return candidate;
    }
    for n in 2..=999 {
        let next = format!("{candidate}-{n}");
        if is_name(&next) && used.insert(next.clone()) {
            return next;
        }
    }
    candidate
}

pub(super) fn auto_mcp_skill_md(
    skill: &str,
    alias: &str,
    desc: Option<&str>,
    instructions: Option<&str>,
    tools: &[(String, String)],
) -> String {
    let description = desc
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Handle requests using tools from the {alias} MCP server"));
    let body = instructions
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let mut lines = vec![
                format!("# {skill}"),
                String::new(),
                format!(
                    "Connected to the `{alias}` MCP server. Call only the required tools with minimal arguments and summarize the results."
                ),
                String::new(),
                "## Tools".to_owned(),
            ];
            for (id, note) in tools {
                if note.is_empty() {
                    lines.push(format!("- `{skill}.{id}`"));
                } else {
                    let short: String = note
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(120)
                        .collect();
                    lines.push(format!("- `{skill}.{id}`: {short}"));
                }
            }
            lines.join("\n")
        });
    render_skill_md(skill, &description, &body)
}

fn target_roles(
    args: &Value,
    snapshot: &Snapshot,
    call: &ManageCall,
) -> Result<Vec<String>, ManageError> {
    if let Some(list) = args.get("roles").and_then(Value::as_array) {
        let roles: Vec<String> = list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if roles.is_empty() {
            return Err(reject("invalid_args", "roles must not be empty"));
        }
        return Ok(roles);
    }
    let requester_roles: Vec<String> = snapshot
        .members
        .get(&call.principal)
        .map(|set| set.iter().cloned().collect())
        .unwrap_or_default();
    if requester_roles.is_empty() {
        return Err(reject(
            "no_roles",
            "no roles found to grant tool scopes — specify roles explicitly",
        ));
    }
    Ok(requester_roles)
}

pub(super) struct AddCtx<'a> {
    pub call: &'a ManageCall,
    pub args: &'a Value,
    pub alias: &'a str,
    pub skill: &'a str,
    pub agent: Option<&'a str>,
    pub desc: Option<&'a str>,
    pub instructions: Option<&'a str>,
    pub roles: &'a [String],
    pub snapshot: &'a Snapshot,
    pub host_path: &'a Path,
    pub rel_dir: &'a Path,
}

impl LiveManager {
    pub(super) fn require_host_path(&self) -> Result<PathBuf, ManageError> {
        self.host_path.clone().ok_or_else(|| {
            reject(
                "host_config_required",
                "BOT_HOST_CONFIG (host.json) must be configured to add or remove MCP servers",
            )
        })
    }

    /// Registers `value` with the leak redactor before storing so failures cannot expose it.
    pub(super) async fn store_secret_value(
        &self,
        profile: &ProfileId,
        name: &SecretName,
        value: &str,
        created_secrets: &mut Vec<SecretName>,
    ) -> Result<(), ManageError> {
        if bot_broker::private::reserved_secret(name) {
            return Err(reject(
                "secret_reserved",
                "host-reserved secret names cannot be used as tool credentials",
            ));
        }
        self.watched.register(value);
        let existed = self
            .secrets_admin
            .list_names(profile, &BTreeSet::from([name.as_str().to_owned()]))
            .await
            .is_ok_and(|set| set.contains(name.as_str()));
        self.secrets_admin
            .set_secret(profile, name, value)
            .await
            .map_err(|m| reject("secret_store_failed", m))?;
        if !existed {
            created_secrets.push(name.clone());
        }
        Ok(())
    }

    async fn rollback_secrets(&self, profile: &ProfileId, created: &[SecretName]) {
        for name in created {
            let _ = self.secrets_admin.unset_secret(profile, name).await;
        }
    }

    pub(super) async fn add_mcp(&self, call: &ManageCall) -> Result<ManageOutcome, ManageError> {
        let host_path = self.require_host_path()?;
        let args: Value = serde_json::from_str(&call.args.0)
            .map_err(|_| reject("invalid_args", "failed to parse request arguments"))?;
        let alias = args
            .get("alias")
            .and_then(Value::as_str)
            .filter(|s| is_name(s))
            .ok_or_else(|| reject("invalid_args", "valid alias is required"))?;
        let transport = args
            .get("transport")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "transport (`http` or `stdio`) is required"))?;
        let skill = args.get("skill").and_then(Value::as_str).unwrap_or(alias);
        if !is_name(skill) {
            return Err(reject("invalid_name", "invalid skill name"));
        }
        let agent = args.get("agent").and_then(Value::as_str);
        let desc = args.get("description").and_then(Value::as_str);
        let instructions = args.get("instructions").and_then(Value::as_str);
        if desc.is_some_and(|d| blocks_output(d, &[]))
            || instructions.is_some_and(|b| blocks_output(b, &[]))
        {
            return Err(reject(
                "secret_in_skill",
                "skill description or instructions appear to contain a secret value",
            ));
        }

        let _guard = self.tx_lock.lock().await;
        let snapshot = self.current_snapshot(&call.profile)?;
        if let Some(target) = agent
            && !snapshot.agents.contains_key(&AgentId(target.to_owned()))
        {
            return Err(reject(
                "unknown_agent",
                format!("agent `{target}` not found"),
            ));
        }
        let roles = target_roles(&args, &snapshot, call)?;
        let rel_dir = skill_rel_dir(agent, skill).map_err(|m| reject("invalid_name", m))?;
        let skill_dir = self.profile_dir(&call.profile).join(&rel_dir);
        if skill_dir.join("tools.yaml").exists() {
            return Err(reject(
                "skill_has_tools",
                format!(
                    "skill `{skill}` already has tools.yaml — choose a new skill name or remove the existing MCP server first"
                ),
            ));
        }
        let host_text = fs::read_to_string(&host_path)
            .map_err(|_| reject("io_error", "failed to read host.json"))?;
        let mut host_doc: Value = serde_json::from_str(&host_text)
            .map_err(|_| reject("invalid_host", "host.json is not valid JSON"))?;
        if host_doc
            .get("servers")
            .and_then(Value::as_object)
            .is_some_and(|m| m.contains_key(alias))
        {
            return Err(reject(
                "server_exists",
                format!("MCP server `{alias}` is already registered in host.json"),
            ));
        }

        let ctx = AddCtx {
            call,
            args: &args,
            alias,
            skill,
            agent,
            desc,
            instructions,
            roles: &roles,
            snapshot: &snapshot,
            host_path: &host_path,
            rel_dir: &rel_dir,
        };
        let mut created_secrets = Vec::new();
        let outcome = match transport {
            "http" => {
                self.add_http_mcp(&ctx, &mut host_doc, &mut created_secrets)
                    .await
            }
            "stdio" => {
                self.add_stdio_mcp(&ctx, &mut host_doc, &mut created_secrets)
                    .await
            }
            _ => Err(reject(
                "invalid_args",
                "transport must be `http` or `stdio`",
            )),
        };
        if outcome.is_err() {
            self.rollback_secrets(&call.profile, &created_secrets).await;
        }
        outcome
    }

    async fn add_http_mcp(
        &self,
        ctx: &AddCtx<'_>,
        host_doc: &mut Value,
        created_secrets: &mut Vec<SecretName>,
    ) -> Result<ManageOutcome, ManageError> {
        let url = ctx
            .args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "url is required for remote HTTP MCP"))?;
        let auth_secret = match ctx.args.get("auth_secret").and_then(Value::as_str) {
            Some(raw) => Some(
                SecretName::parse(raw)
                    .filter(|n| !bot_broker::private::reserved_secret(n))
                    .ok_or_else(|| reject("invalid_secret", "invalid auth_secret name"))?,
            ),
            None => None,
        };
        if let Some(secret_val) = ctx.args.get("secret_value").and_then(Value::as_str) {
            let Some(name) = &auth_secret else {
                return Err(reject(
                    "invalid_args",
                    "auth_secret must be set when supplying secret_value",
                ));
            };
            self.store_secret_value(&ctx.call.profile, name, secret_val, created_secrets)
                .await?;
        }
        let remote = Remote::parse(url, auth_secret.clone())
            .map_err(|why| reject("invalid_url", format!("invalid url: {why}")))?;

        let (mut credentials, mut bearer) = (Vec::new(), None);
        if let Some(name) = &auth_secret {
            self.secrets_admin.invalidate(&ctx.call.profile);
            let handle = self
                .watched
                .for_profile(ctx.call.profile.clone())
                .await
                .map_err(|_| reject("secret_unavailable", "failed to open secret store"))?;
            let value = handle.resolve(name.clone()).await.map_err(|_| {
                reject(
                    "secret_missing",
                    format!(
                        "secret `{}` is missing for profile `{}` in store ({}) — store it first or pass `secret_value`",
                        name.as_str(),
                        ctx.call.profile.0,
                        self.secrets_admin.backend_name()
                    ),
                )
            })?;
            let text = String::from_utf8(value.0.clone())
                .ok()
                .filter(|t| !t.is_empty() && !t.contains(['\0', '\r', '\n']))
                .ok_or_else(|| {
                    reject(
                        "secret_invalid",
                        "secret value is not valid for a bearer header",
                    )
                })?;
            credentials.push(value.0);
            bearer = Some(text);
        }

        let wire = Arc::new(
            ReqwestWire::new(Arc::new(SystemResolver)).map_err(|m| reject("wire_failed", m))?,
        );
        let mut http = Http::new(wire, remote.clone(), bearer, false);
        let listed = async {
            http.connect(SYNC_LIMIT).await?;
            http.tools(SYNC_LIMIT).await
        }
        .await;
        http.close().await;
        let listed = listed.map_err(|e| {
            reject(
                "mcp_unreachable",
                format!(
                    "failed to list tools from remote MCP server: {}",
                    mcp_sync::describe(&e)
                ),
            )
        })?;

        let only: Option<BTreeSet<String>> = ctx
            .args
            .get("only_tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
        let default_class = ctx
            .args
            .get("default_class")
            .and_then(Value::as_str)
            .unwrap_or("write");
        let class_map = ctx.args.get("tool_classes").and_then(Value::as_object);

        let mut used_ids = BTreeSet::new();
        let mut wanted: Vec<(String, String)> = Vec::new();
        for item in &listed {
            let Some(mcp_name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            if only
                .as_ref()
                .is_some_and(|filter| !filter.contains(mcp_name))
            {
                continue;
            }
            wanted.push((
                sanitize_tool_id(mcp_name, &mut used_ids),
                mcp_name.to_owned(),
            ));
        }
        if wanted.is_empty() {
            return Err(reject(
                "no_tools_found",
                "no matching tools were returned by the server",
            ));
        }

        let entries = mcp_sync::entries_of(&wanted, &listed, &credentials)
            .map_err(|problems| reject("mcp_tools_invalid", problems.join("\n")))?;
        let lock_text = lock::render(ctx.alias, &entries)
            .ok_or_else(|| reject("mcp_lock_failed", "failed to render tools.lock.yaml"))?;

        let target_egress = remote.target_text();
        let mut tools_list = Vec::new();
        let mut new_scopes = Vec::new();
        let mut summary_tools = Vec::new();
        for (id, entry) in &entries {
            let class = class_map
                .and_then(|m| m.get(&entry.name).or_else(|| m.get(id)))
                .and_then(Value::as_str)
                .unwrap_or(default_class);
            let scope = format!("{}.{id}", ctx.skill);
            new_scopes.push(scope.clone());
            summary_tools.push((id.clone(), entry.description.clone()));
            let mut item = Map::new();
            item.insert("id".into(), json!(id));
            if entry.name != *id {
                item.insert("name".into(), json!(entry.name));
            }
            item.insert("class".into(), json!(class));
            item.insert("scopes".into(), json!([scope]));
            item.insert("external".into(), json!(true));
            item.insert("egress".into(), json!([target_egress]));
            tools_list.push(Value::Object(item));
        }
        let tools_yaml = render_yaml(&json!({
            "version": 1,
            "server": ctx.alias,
            "tools": tools_list,
        }));

        let mut server_entry = json!({"transport": "http", "url": url});
        if let Some(secret) = &auth_secret {
            server_entry["auth"] = json!({"kind": "bearer", "secret": secret.as_str()});
        }
        host_add_server(
            host_doc,
            ctx.alias,
            server_entry,
            &ctx.call.profile.0,
            std::slice::from_ref(&target_egress),
        )
        .map_err(|m| reject("invalid_host", m))?;

        let profile_dir = self.profile_dir(&ctx.call.profile);
        let profile_yaml_path = profile_dir.join("profile.yaml");
        let profile_text = fs::read_to_string(&profile_yaml_path)
            .map_err(|_| reject("io_error", "failed to read profile.yaml"))?;
        let mut profile_doc =
            bot_profile::parse_yaml("profile.yaml", &profile_text, &Limits::default())
                .map_err(|e| reject("invalid_profile", e.to_string()))?;
        profile_add_scopes_and_secrets(
            &mut profile_doc,
            ctx.roles,
            ctx.agent,
            &ctx.snapshot.root.0,
            &new_scopes,
            &[],
        )
        .map_err(|m| reject("invalid_profile", m))?;

        let mut txn = DiskTxn::new();
        let skill_dir = profile_dir.join(ctx.rel_dir);
        if !skill_dir.join("SKILL.md").exists() {
            let md = auto_mcp_skill_md(
                ctx.skill,
                ctx.alias,
                ctx.desc,
                ctx.instructions,
                &summary_tools,
            );
            txn.write_file(&skill_dir.join("SKILL.md"), md.as_bytes())
                .map_err(|m| reject("io_error", m))?;
        }
        txn.write_file(&skill_dir.join("tools.yaml"), tools_yaml.as_bytes())
            .map_err(|m| reject("io_error", m))?;
        txn.write_file(&skill_dir.join(lock::FILE), lock_text.as_bytes())
            .map_err(|m| reject("io_error", m))?;
        txn.write_file(&profile_yaml_path, render_yaml(&profile_doc).as_bytes())
            .map_err(|m| reject("io_error", m))?;
        let host_rendered = serde_json::to_string_pretty(host_doc)
            .map_err(|_| reject("invalid_host", "failed to serialize host.json"))?;
        txn.write_file(ctx.host_path, format!("{host_rendered}\n").as_bytes())
            .map_err(|m| reject("io_error", m))?;

        let new_rev = self.activate_disk(&ctx.call.profile, txn).await?;
        let tool_ids: Vec<String> = entries
            .keys()
            .map(|id| format!("{}.{id}", ctx.skill))
            .collect();
        let payload = json!({
            "ok": true,
            "action": "mcp.add",
            "transport": "http",
            "alias": ctx.alias,
            "skill": ctx.skill,
            "agent": ctx.agent,
            "tools": tool_ids,
            "auth_secret": auth_secret.as_ref().map(|s| s.as_str()),
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }
}
