//! `skill.inspect`: 슈퍼 관리자에게 현재 프로필의 에이전트 구조, 스킬 목록·용량, MCP 서버, 비밀 이름(값 제외)을 조회해 돌려준다.
use super::{LiveManager, yaml_edit::agent_of_skill_path};
use bot_core::{
    command::JsonPayload,
    manage::{ManageCall, ManageError, ManageOutcome},
};
use bot_profile::SKILL_MAX_BYTES;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

impl LiveManager {
    pub(super) async fn inspect(&self, call: &ManageCall) -> Result<ManageOutcome, ManageError> {
        let args: Value =
            serde_json::from_str(&call.args.0).map_err(|_| ManageError::Rejected {
                code: "invalid_args",
                message: "요청 인자를 읽지 못했어요".into(),
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
                message: "활성 프로필을 찾지 못했어요".into(),
            })?;

        // 스킬 중복 제거(상속으로 여러 에이전트에 보이는 스킬은 원본 경로 기준으로 한 번만 센다)
        let mut unique_skills: BTreeMap<String, (&String, &bot_core::profile::Skill)> =
            BTreeMap::new();
        let mut server_skills: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for agent in snapshot.agents.values() {
            for (name, skill) in &agent.skills {
                let path = skill.instructions.relative_path.clone();
                unique_skills.entry(path.clone()).or_insert((name, skill));
                for tool in skill.tools.values() {
                    server_skills
                        .entry(tool.server.clone())
                        .or_default()
                        .insert(name.clone());
                }
            }
        }

        let mut agents_out = Vec::new();
        for (id, agent) in &snapshot.agents {
            let own_skills: Vec<&str> = agent
                .skills
                .iter()
                .filter(|(_, s)| {
                    agent_of_skill_path(&s.instructions.relative_path).as_deref()
                        == Some(id.0.as_str())
                })
                .map(|(name, _)| name.as_str())
                .collect();
            let used: usize = agent
                .skills
                .values()
                .map(|s| s.instructions.text.len())
                .sum();
            agents_out.push(json!({
                "id": id.0,
                "parent": agent.parent.as_ref().map(|p| &p.0),
                "description": agent.description,
                "skills": agent.skills.keys().collect::<Vec<_>>(),
                "own_skills": own_skills,
                "skill_bytes_used": used,
                "skill_bytes_limit": SKILL_MAX_BYTES,
                "skill_bytes_remaining": SKILL_MAX_BYTES.saturating_sub(used),
            }));
        }

        let mut skills_out = Vec::new();
        let mut detail = None;
        for (rel_path, (name, skill)) in &unique_skills {
            let owner = agent_of_skill_path(rel_path);
            let dir_path = rel_path.strip_suffix("/SKILL.md").unwrap_or(rel_path);
            let tools: Vec<Value> = skill
                .tools
                .iter()
                .map(|(id, tool)| {
                    json!({
                        "id": id.0,
                        "server": tool.server,
                        "class": format!("{:?}", tool.class).to_ascii_lowercase(),
                        "scopes": tool.scopes,
                    })
                })
                .collect();
            skills_out.push(json!({
                "name": name,
                "agent": owner,
                "path": dir_path,
                "description": skill.description,
                "body_bytes": skill.instructions.text.len(),
                "tools": tools,
            }));
            if filter_skill == Some(name.as_str())
                && (filter_agent.is_none() || filter_agent == owner.as_deref())
            {
                detail = Some(json!({
                    "name": name,
                    "agent": owner,
                    "path": dir_path,
                    "description": skill.description,
                    "body": skill.instructions.text,
                    "tools": tools,
                }));
            }
        }

        let (mcp_servers, declared_remote_secrets) = {
            let host = self.host.lock().unwrap_or_else(|e| e.into_inner());
            let mut list = Vec::new();
            let mut remote_secrets = BTreeSet::new();
            for (alias, remote) in &host.remote_servers {
                if let Some(secret) = &remote.bearer {
                    remote_secrets.insert(secret.as_str().to_owned());
                }
                let scheme = if remote.tls { "https" } else { "http" };
                let url = format!("{scheme}://{}:{}{}", remote.host, remote.port, remote.path);
                list.push(json!({
                    "alias": alias,
                    "transport": "http",
                    "url": url,
                    "target": remote.target_text(),
                    "auth_secret": remote.bearer.as_ref().map(|s| s.as_str()),
                    "used_by_skills": server_skills.get(alias).cloned().unwrap_or_default(),
                }));
            }
            for (alias, spec) in &host.servers {
                list.push(json!({
                    "alias": alias,
                    "transport": "stdio",
                    "root": spec.root.display().to_string(),
                    "command": spec.command,
                    "used_by_skills": server_skills.get(alias).cloned().unwrap_or_default(),
                }));
            }
            (list, remote_secrets)
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

        let mut body = json!({
            "ok": true,
            "profile": call.profile.0,
            "revision": snapshot.revision.0,
            "root_agent": snapshot.root.0,
            "roles": snapshot.roles.keys().collect::<Vec<_>>(),
            "agents": agents_out,
            "skills": skills_out,
            "mcp_servers": mcp_servers,
            "secrets": {
                "backend": self.secrets_admin.backend_name(),
                "declared": snapshot.secrets.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "stored": stored,
            },
        });
        if let Some(found) = detail {
            body["skill_detail"] = found;
        }
        Ok(ManageOutcome {
            payload: JsonPayload(body.to_string()),
            activated: None,
        })
    }
}
