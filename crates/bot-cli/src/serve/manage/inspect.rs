//! `skill.inspect` handler returning summarized profile structure or filtered agent/skill details within the model output budget.
use super::{LiveManager, yaml_edit::agent_of_skill_path};
use bot_core::{
    command::JsonPayload,
    manage::{ManageCall, ManageError, ManageOutcome},
};
use bot_profile::SKILL_MAX_BYTES;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const DESC_SUMMARY_CHARS: usize = 60;

fn clip_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

fn format_inspect(
    profile: &str,
    snapshot: &bot_core::profile::Snapshot,
    mcp_servers: Value,
    backend_name: &str,
    stored: &BTreeSet<String>,
    filter_skill: Option<&str>,
    filter_agent: Option<&str>,
) -> Value {
    // Deduplicate inherited skills by source relative path so each skill is counted once.
    let mut unique_skills: BTreeMap<String, (&String, &bot_core::profile::Skill)> = BTreeMap::new();
    for agent in snapshot.agents.values() {
        for (name, skill) in &agent.skills {
            let path = skill.instructions.relative_path.clone();
            unique_skills.entry(path).or_insert((name, skill));
        }
    }

    // Return only the requested skill so its body and tool list stay under the 8,000-char model clip limit.
    if let Some(target_skill) = filter_skill {
        let mut detail = None;
        for (rel_path, (name, skill)) in &unique_skills {
            if name.as_str() != target_skill {
                continue;
            }
            let owner = agent_of_skill_path(rel_path);
            if filter_agent.is_some() && filter_agent != owner.as_deref() {
                continue;
            }
            let dir_path = rel_path.strip_suffix("/SKILL.md").unwrap_or(rel_path);
            let servers: BTreeSet<&str> = skill
                .tools
                .values()
                .map(|tool| tool.server.as_str())
                .collect();
            let tool_ids: Vec<&str> = skill.tools.keys().map(|id| id.0.as_str()).collect();
            detail = Some(json!({
                "name": name,
                "agent": owner,
                "path": dir_path,
                "description": skill.description,
                "body_bytes": skill.instructions.text.len(),
                "body": skill.instructions.text,
                "tool_count": tool_ids.len(),
                "servers": servers,
                "tool_ids": tool_ids,
            }));
            break;
        }
        return match detail {
            Some(found) => json!({
                "ok": true,
                "profile": profile,
                "revision": snapshot.revision.0,
                "skill_detail": found,
            }),
            None => json!({
                "ok": true,
                "profile": profile,
                "revision": snapshot.revision.0,
                "skill_detail": null,
                "available_skills": unique_skills.values().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            }),
        };
    }

    let mut agents_out = Vec::new();
    for (id, agent) in &snapshot.agents {
        if filter_agent.is_some() && filter_agent != Some(id.0.as_str()) {
            continue;
        }
        let used: usize = agent
            .skills
            .values()
            .map(|s| s.instructions.text.len())
            .sum();
        agents_out.push(json!({
            "id": id.0,
            "description": clip_summary(&agent.description, DESC_SUMMARY_CHARS),
            "bytes_used": used,
            "bytes_left": SKILL_MAX_BYTES.saturating_sub(used),
        }));
    }

    let mut skills_out = Vec::new();
    for (rel_path, (name, skill)) in &unique_skills {
        let owner = agent_of_skill_path(rel_path);
        if filter_agent.is_some() && filter_agent != owner.as_deref() {
            continue;
        }
        let servers: BTreeSet<&str> = skill
            .tools
            .values()
            .map(|tool| tool.server.as_str())
            .collect();
        let mut item = json!({
            "name": name,
            "agent": owner,
            "body_bytes": skill.instructions.text.len(),
            "tool_count": skill.tools.len(),
        });
        if !servers.is_empty() {
            item["servers"] = json!(servers);
        }
        if let Some(desc) = &skill.description {
            let short = clip_summary(desc, DESC_SUMMARY_CHARS);
            if !short.is_empty() {
                item["description"] = json!(short);
            }
        }
        skills_out.push(item);
    }

    // Return only the requested agent and its skills when filtered by agent.
    if let Some(target_agent) = filter_agent {
        return json!({
            "ok": true,
            "profile": profile,
            "revision": snapshot.revision.0,
            "agent": target_agent,
            "skill_bytes_limit": SKILL_MAX_BYTES,
            "agents": agents_out,
            "skills": skills_out,
        });
    }

    let declared: Vec<&str> = snapshot.secrets.iter().map(|s| s.as_str()).collect();
    let missing: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|name| !stored.contains(*name))
        .collect();

    // Full summary sized to fit within the 8,000-char model tool output budget.
    json!({
        "ok": true,
        "profile": profile,
        "revision": snapshot.revision.0,
        "root_agent": snapshot.root.0,
        "skill_bytes_limit": SKILL_MAX_BYTES,
        "roles": snapshot.roles.keys().collect::<Vec<_>>(),
        "agents": agents_out,
        "skills": skills_out,
        "mcp_servers": mcp_servers,
        "secrets": {
            "backend": backend_name,
            "declared": declared,
            "missing": missing,
        },
    })
}

impl LiveManager {
    pub(super) async fn inspect(&self, call: &ManageCall) -> Result<ManageOutcome, ManageError> {
        let args: Value =
            serde_json::from_str(&call.args.0).map_err(|_| ManageError::Rejected {
                code: "invalid_args",
                message: "failed to parse request arguments".into(),
            })?;
        let filter_skill = args.get("skill").and_then(Value::as_str);
        let filter_agent = args.get("agent").and_then(Value::as_str);

        let snapshot = self
            .registry
            .set()
            .profile(&call.profile)
            .cloned()
            .ok_or_else(|| ManageError::Rejected {
                code: "unknown_profile",
                message: "active profile not found".into(),
            })?;

        let (mcp_servers, stored) = if filter_skill.is_none() && filter_agent.is_none() {
            let (mcp_servers, declared_remote_secrets) = {
                let host = self.host.lock().unwrap_or_else(|e| e.into_inner());
                let mut http_list = Vec::new();
                let mut remote_secrets = BTreeSet::new();
                for (alias, remote) in &host.remote_servers {
                    if let Some(secret) = &remote.bearer {
                        remote_secrets.insert(secret.as_str().to_owned());
                    }
                    let scheme = if remote.tls { "https" } else { "http" };
                    let url = format!("{scheme}://{}:{}{}", remote.host, remote.port, remote.path);
                    http_list.push(json!({
                        "alias": alias,
                        "url": url,
                        "auth_secret": remote.bearer.as_ref().map(|s| s.as_str()),
                        "auth_oauth": remote.oauth.as_ref().map(|s| s.as_str()),
                    }));
                }
                let stdio_list: Vec<&str> = host.servers.keys().map(String::as_str).collect();
                (
                    json!({ "stdio": stdio_list, "http": http_list }),
                    remote_secrets,
                )
            };

            let mut candidates: BTreeSet<String> = snapshot
                .secrets
                .iter()
                .map(|s| s.as_str().to_owned())
                .collect();
            candidates.extend(declared_remote_secrets);
            let stored = self
                .secrets_admin
                .list_names(&call.profile, &candidates)
                .await
                .unwrap_or_default();
            (mcp_servers, stored)
        } else {
            (Value::Null, BTreeSet::new())
        };

        let body = format_inspect(
            &call.profile.0,
            &snapshot,
            mcp_servers,
            self.secrets_admin.backend_name(),
            &stored,
            filter_skill,
            filter_agent,
        );
        Ok(ManageOutcome {
            payload: JsonPayload(body.to_string()),
            activated: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{
        command::JsonPayload,
        id::{AgentId, ProfileId, Revision, SecretName, ToolId},
        profile::{Agent, Document, Policy, Skill, Snapshot, Tool},
        state::Risk,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn large_snapshot() -> Snapshot {
        let mut agents = BTreeMap::new();
        let mut secrets = Vec::new();
        for i in 0..30 {
            secrets.push(SecretName::parse(&format!("secret-{i:02}")).unwrap());
        }
        agents.insert(
            AgentId("main".into()),
            Agent {
                description: "Coordinator".into(),
                builtin: BTreeSet::new(),
                parent: None,
                model: "fast".into(),
                instructions: String::new(),
                env: BTreeMap::new(),
                scopes: BTreeSet::from(["run".into()]),
                skills: BTreeMap::new(),
                knowledge: Vec::new(),
                memory_seeds: Vec::new(),
                triggers: Vec::new(),
            },
        );
        for a in 0..12 {
            let agent_id = format!("main/sub-{a:02}");
            let mut skills = BTreeMap::new();
            for s in 0..2 {
                let skill_name = format!("skill-{a:02}-{s}");
                let server = format!("srv-{a:02}-{s}");
                let mut tools = BTreeMap::new();
                for t in 0..17 {
                    let tool_id = format!("{skill_name}.tool-{t:02}");
                    tools.insert(
                        ToolId(tool_id),
                        Tool {
                            server: server.clone(),
                            class: Risk::Read,
                            external: false,
                            effects: false,
                            timeout_secs: Some(60),
                            destination: None,
                            scopes: BTreeSet::from([format!("{skill_name}.read")]),
                            env: BTreeSet::new(),
                            secrets: BTreeMap::new(),
                            egress: BTreeSet::new(),
                            input: JsonPayload("{}".into()),
                            description: None,
                            definition: None,
                            mcp_name: None,
                            revision: Revision("rev".into()),
                        },
                    );
                }
                skills.insert(
                    skill_name.clone(),
                    Skill {
                        description: Some(format!("{skill_name} description")),
                        instructions: Document {
                            relative_path: format!(
                                "agents/main/agents/sub-{a:02}/skills/{skill_name}/SKILL.md"
                            ),
                            text: format!("# {skill_name}\nBody content"),
                        },
                        tools,
                    },
                );
            }
            agents.insert(
                AgentId(agent_id),
                Agent {
                    description: format!("Sub-agent {a:02} role description"),
                    builtin: BTreeSet::new(),
                    parent: Some(AgentId("main".into())),
                    model: "fast".into(),
                    instructions: String::new(),
                    env: BTreeMap::new(),
                    scopes: BTreeSet::from(["run".into()]),
                    skills,
                    knowledge: Vec::new(),
                    memory_seeds: Vec::new(),
                    triggers: Vec::new(),
                },
            );
        }
        Snapshot {
            id: ProfileId("workspace".into()),
            revision: Revision("0123456789abcdef".into()),
            policy_revision: Revision("fedcba9876543210".into()),
            timezone: "Asia/Seoul".into(),
            model: "fast".into(),
            root: AgentId("main".into()),
            agents,
            roles: BTreeMap::from([("admin".into(), BTreeSet::from(["run".into()]))]),
            members: BTreeMap::new(),
            super_admins: BTreeSet::new(),
            bindings: BTreeMap::new(),
            policy: Policy {
                scopes: BTreeSet::from(["run".into()]),
                agents: BTreeMap::new(),
                approve: BTreeSet::new(),
            },
            secrets,
        }
    }

    #[test]
    fn large_profile_summary_and_detail_stay_under_model_clip_limit() {
        let snap = large_snapshot();
        let stdio_servers: Vec<String> = (0..24)
            .map(|i| format!("srv-{:02}-{}", i / 2, i % 2))
            .collect();
        let servers = json!({ "stdio": stdio_servers, "http": [] });
        let stored: BTreeSet<String> = snap.secrets.iter().map(|s| s.as_str().to_owned()).collect();

        // Full summary (13 agents, 24 skills, 408 tools, 24 servers, 30 secrets) must stay under 6,500 chars.
        let summary = format_inspect("workspace", &snap, servers, "keychain", &stored, None, None);
        let summary_text = summary.to_string();
        assert!(
            summary_text.chars().count() < 6_500,
            "summary exceeded 6,500-char budget: {} chars",
            summary_text.chars().count()
        );
        assert!(summary["skills"].as_array().unwrap().len() == 24);
        assert!(summary["mcp_servers"]["stdio"].as_array().unwrap().len() == 24);
        assert!(summary["secrets"]["missing"].as_array().unwrap().is_empty());

        // Skill detail query returns only skill_detail so the body is not clipped.
        let empty_stored = BTreeSet::new();
        let detail = format_inspect(
            "workspace",
            &snap,
            Value::Null,
            "keychain",
            &empty_stored,
            Some("skill-11-1"),
            None,
        );
        let detail_text = detail.to_string();
        assert!(
            detail_text.chars().count() < 2_000,
            "skill detail exceeded 2,000-char budget: {} chars",
            detail_text.chars().count()
        );
        assert_eq!(detail["skill_detail"]["name"], "skill-11-1");
        assert_eq!(detail["skill_detail"]["tool_count"], 17);
        assert!(detail.get("mcp_servers").is_none());

        // Unknown skill query returns null detail plus available skill names.
        let missing = format_inspect(
            "workspace",
            &snap,
            Value::Null,
            "keychain",
            &empty_stored,
            Some("no-such-skill"),
            None,
        );
        assert!(missing["skill_detail"].is_null());
        assert_eq!(missing["available_skills"].as_array().unwrap().len(), 24);

        // Agent filter returns only that agent and its skills without server/secret bloat.
        let agent_view = format_inspect(
            "workspace",
            &snap,
            Value::Null,
            "keychain",
            &empty_stored,
            None,
            Some("main/sub-03"),
        );
        assert_eq!(agent_view["agent"], "main/sub-03");
        assert_eq!(agent_view["agents"].as_array().unwrap().len(), 1);
        assert_eq!(agent_view["skills"].as_array().unwrap().len(), 2);
        assert!(agent_view.get("mcp_servers").is_none());
    }
}
