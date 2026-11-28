//! Activation checks validating profile aliases against the host catalog.
use crate::{hint, host::HostCatalog, issue::Issues};
use bot_core::profile::{Adapter, Snapshot};
use std::collections::BTreeSet;

fn agent_file(id: &str) -> String {
    let parts: Vec<&str> = id.split('/').collect();
    format!("agents/{}/AGENT.md", parts.join("/agents/"))
}

fn model_places(snap: &Snapshot, alias: &str) -> Vec<String> {
    let mut places = Vec::new();
    if snap.model == alias {
        places.push("profile.yaml".to_owned());
    }
    for (id, agent) in &snap.agents {
        let inherited = match &agent.parent {
            Some(parent) => snap.agents.get(parent).is_some_and(|p| p.model == alias),
            None => snap.model == alias,
        };
        if agent.model == alias && !inherited {
            places.push(agent_file(&id.0));
        }
    }
    if places.is_empty() {
        places.push("profile.yaml".to_owned());
    }
    places
}

/// Webhook alias `v1` conflicts with the OpenAI `/v1` route when any OpenAI endpoint is configured.
fn http_endpoint(host: &HostCatalog, endpoint: &str, openai: bool, at: &str, issues: &mut Issues) {
    let (is_webhook, is_openai) = (
        host.http_endpoints.contains(endpoint),
        host.openai_endpoints.contains_key(endpoint),
    );
    if !is_webhook && !is_openai {
        issues.error_at(
            "unknown-endpoint",
            "profile.yaml",
            at,
            format!("HTTP endpoint is not registered on this host: {endpoint}"),
        );
    } else if openai && !is_openai {
        issues.error_at(
            "endpoint-kind-mismatch",
            "profile.yaml",
            at,
            format!("HTTP endpoint `{endpoint}` is registered as a webhook (`kind: webhook`). OpenAI bindings with `model` require a `kind: openai` endpoint."),
        );
    } else if !openai && !is_webhook {
        issues.error_at(
            "endpoint-kind-mismatch",
            "profile.yaml",
            at,
            format!("HTTP endpoint `{endpoint}` is registered as OpenAI-compatible (`kind: openai`). Webhook bindings without `model` require a webhook endpoint."),
        );
    }
    if !openai && endpoint == "v1" && !host.openai_endpoints.is_empty() {
        issues.error_at(
            "webhook-v1-conflict",
            "profile.yaml",
            at,
            "Webhook endpoint alias `v1` conflicts with the OpenAI-compatible `/v1` route and cannot be enabled alongside OpenAI endpoints. Rename the webhook alias (e.g. `hooks`) in both host config and profile `endpoint`.",
        );
    }
}

fn remote_tool(
    host: &HostCatalog,
    profile: &str,
    id: &str,
    tool: &bot_core::profile::Tool,
    target: &str,
    file: &str,
    issues: &mut Issues,
) {
    if tool.effects {
        issues.error(
            "remote-effects",
            file,
            format!("{id}: remote server tools cannot enable `effects: true` (effect ledger is local-only)"),
        );
    }
    if !tool.secrets.is_empty() || !tool.env.is_empty() {
        issues.error(
            "remote-tool-config",
            file,
            format!("{id}: remote server tools cannot declare `secrets` or `env` (configure auth via host `auth`)"),
        );
    }
    if !host
        .effective_egress(profile, &tool.egress)
        .contains(target)
    {
        issues.error(
            "remote-egress",
            file,
            format!("{id}: remote server target `{target}` is missing from tool `egress` (intersected with host `egress.{profile}`); declare the exact `host:port`"),
        );
    }
}

fn timeout_cap(
    host: &HostCatalog,
    id: &str,
    tool: &bot_core::profile::Tool,
    file: &str,
    issues: &mut Issues,
) {
    let (cap, secs) = (host.tool_call_cap(), tool.call_secs());
    if secs <= cap {
        return;
    }
    let fix = if tool.timeout_secs.is_some() {
        format!(
            "reduce tool `timeout_secs` to <= {cap} or raise host `limits.tool_call_max_secs` to >= {secs}"
        )
    } else {
        format!(
            "omitted `timeout_secs` defaults to {secs}s; set `timeout_secs` <= {cap} on this tool or raise host `limits.tool_call_max_secs` to >= {secs}"
        )
    };
    issues.error(
        "tool-timeout-over-cap",
        file,
        format!("{id}: effective timeout {secs}s exceeds host cap {cap}s ({fix})"),
    );
}

pub fn check_host(snap: &Snapshot, host: &HostCatalog, issues: &mut Issues) {
    let profile = snap.id.0.as_str();
    let mut models: BTreeSet<&str> = BTreeSet::from([snap.model.as_str()]);
    models.extend(snap.agents.values().map(|a| a.model.as_str()));
    let allowed = host.models_of(profile);
    for model in models
        .into_iter()
        .filter(|m| !host.allows_model(profile, m))
    {
        let fix = if allowed.is_empty() {
            format!(
                "`models.{profile}` in host config (BOT_HOST_CONFIG) is empty. Example: `\"models\": {{\"{profile}\": [\"{model}\"]}}`"
            )
        } else {
            let meant = hint::nearest(model, allowed.iter().copied())
                .map_or(String::new(), |m| format!(" Did you mean `{m}`?"));
            format!(
                "Allowed aliases: {}.{meant} To allow a new alias, add `{model}` to `models.{profile}` in host config (BOT_HOST_CONFIG).",
                allowed.join(", ")
            )
        };
        let message =
            format!("Model alias `{model}` is not allowed for profile `{profile}`. {fix}");
        for place in model_places(snap, model) {
            issues.error_at("model-not-allowed", &place, "/model", message.clone());
        }
    }
    for admin in snap
        .super_admins
        .iter()
        .filter(|id| !host.allows_super_admin(profile, &id.0))
    {
        issues.error_at(
            "super-admin-not-allowed",
            "profile.yaml",
            "/super_admins",
            format!(
                "Super admin `{}` is not allowed by host config (`super_admins.{profile}`)",
                admin.0
            ),
        );
    }
    for (id, binding) in &snap.bindings {
        let at = format!("binding {}", id.0);
        match &binding.source {
            Adapter::Cli { account } if !host.cli_accounts.contains(account) => {
                issues.error_at(
                    "unknown-account",
                    "profile.yaml",
                    &at,
                    format!("CLI account is not registered on this host: {account}"),
                );
            }
            Adapter::SlackChannel {
                workspace, channel, ..
            } => match host.slack.get(workspace) {
                None => issues.error_at(
                    "unknown-workspace",
                    "profile.yaml",
                    &at,
                    format!("Slack workspace is not registered on this host: {workspace}"),
                ),
                Some(ws) if !ws.channels.contains(channel) => {
                    issues.error_at(
                        "unknown-channel",
                        "profile.yaml",
                        &at,
                        format!("Channel is not registered in workspace {workspace}: {channel}"),
                    );
                }
                Some(_) => {}
            },
            Adapter::SlackDm { workspace } => match host.slack.get(workspace) {
                None => issues.error_at(
                    "unknown-workspace",
                    "profile.yaml",
                    &at,
                    format!("Slack workspace is not registered on this host: {workspace}"),
                ),
                Some(ws) if !ws.dm => {
                    issues.error_at(
                        "dm-not-allowed",
                        "profile.yaml",
                        &at,
                        format!("Workspace {workspace} does not allow DM ingress"),
                    );
                }
                Some(_) => {}
            },
            Adapter::TelegramDm { bot } => match host.telegram.get(bot) {
                None => issues.error_at(
                    "unknown-bot",
                    "profile.yaml",
                    &at,
                    format!("Telegram bot is not registered on this host: {bot}"),
                ),
                Some(entry) if !entry.dm => {
                    issues.error_at(
                        "dm-not-allowed",
                        "profile.yaml",
                        &at,
                        format!("Telegram bot {bot} does not allow DM ingress"),
                    );
                }
                Some(_) => {}
            },
            Adapter::Cli { .. } => {}
            Adapter::Http { endpoint } => http_endpoint(host, endpoint, false, &at, issues),
            Adapter::OpenAi {
                endpoint,
                approvers,
                ..
            } => {
                http_endpoint(host, endpoint, true, &at, issues);
                // Warning rather than error: revoking a member's last key slot must not block
                // re-activation and leave previously active config running.
                if let Some(subjects) = host.openai_endpoints.get(endpoint) {
                    let prefix = format!("http:{endpoint}/");
                    for requester in approvers.keys() {
                        let subject = requester.0.strip_prefix(&prefix);
                        if !subject.is_some_and(|s| subjects.contains(s)) {
                            issues.warning(
                                "approver-key-unknown",
                                "profile.yaml",
                                format!(
                                    "{at}: no key slot on endpoint `{endpoint}` maps to HTTP principal `{}` (check `keys.<slot>.subject` in host config)",
                                    requester.0
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    for skill in snap.agents.values().flat_map(|a| a.skills.values()) {
        let file = skill
            .instructions
            .relative_path
            .strip_suffix("SKILL.md")
            .map_or_else(String::new, |dir| format!("{dir}tools.yaml"));
        if !seen.insert(file.clone()) {
            continue;
        }
        for (id, tool) in &skill.tools {
            if !host.servers.contains(&tool.server) {
                issues.error(
                    "missing-server",
                    &file,
                    format!(
                        "{}: server is not installed on this host: {}",
                        id.0, tool.server
                    ),
                );
            }
            if let Some(target) = host.remote_servers.get(&tool.server) {
                remote_tool(host, profile, id.0.as_str(), tool, target, &file, issues);
            }
            timeout_cap(host, id.0.as_str(), tool, &file, issues);
            let kept = host.effective_egress(profile, &tool.egress);
            if kept.len() < tool.egress.len() {
                let dropped: Vec<&String> = tool.egress.difference(&kept).collect();
                issues.warning(
                    "egress-narrowed",
                    &file,
                    format!("{}: egress host(s) blocked by host cap: {dropped:?}", id.0),
                );
            }
        }
    }
}
