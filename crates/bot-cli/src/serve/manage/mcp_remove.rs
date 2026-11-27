//! Live `mcp.remove` handler pruning MCP servers, tool bindings, unused scopes, and optional secrets.
use super::{
    LiveManager,
    mcp::reject,
    yaml_edit::{DiskTxn, agent_of_skill_path, host_remove_server, profile_prune, render_yaml},
};
use bot_core::{
    command::JsonPayload,
    id::{SecretName, is_name},
    manage::{ManageCall, ManageError, ManageOutcome},
};
use bot_profile::{Limits, lock};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
};

impl LiveManager {
    pub(super) async fn remove_mcp(&self, call: &ManageCall) -> Result<ManageOutcome, ManageError> {
        let host_path = self.require_host_path()?;
        let args: Value = serde_json::from_str(&call.args.0)
            .map_err(|_| reject("invalid_args", "failed to parse request arguments"))?;
        let alias = args
            .get("alias")
            .and_then(Value::as_str)
            .filter(|s| is_name(s))
            .ok_or_else(|| reject("invalid_args", "a valid alias is required"))?;
        let filter_skill = args.get("skill").and_then(Value::as_str);
        let filter_agent = args.get("agent").and_then(Value::as_str);
        let delete_skill = args
            .get("delete_skill")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let remove_secrets = args
            .get("remove_secrets")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let _guard = self.tx_lock.lock().await;
        let set = self.registry.set();
        let snapshot = set
            .profile(&call.profile)
            .cloned()
            .ok_or_else(|| reject("unknown_profile", "active profile not found"))?;
        let host_text = fs::read_to_string(&host_path)
            .map_err(|_| reject("io_error", "failed to read host.json"))?;
        let mut host_doc: Value = serde_json::from_str(&host_text)
            .map_err(|_| reject("invalid_host", "host.json is not valid JSON"))?;
        let in_host = host_doc
            .get("servers")
            .and_then(Value::as_object)
            .is_some_and(|m| m.contains_key(alias));

        let mut matched_dirs: BTreeMap<String, String> = BTreeMap::new();
        let mut removed_scopes = BTreeSet::new();
        let mut remaining_scopes = BTreeSet::new();
        let mut removed_secrets = BTreeSet::new();
        let mut remaining_secrets = BTreeSet::new();
        for agent in snapshot.agents.values() {
            for (name, skill) in &agent.skills {
                let rel = &skill.instructions.relative_path;
                let dir = rel.strip_suffix("/SKILL.md").unwrap_or(rel).to_owned();
                let owner = agent_of_skill_path(rel);
                let uses_alias = skill.tools.values().any(|t| t.server == alias);
                let selected = uses_alias
                    && filter_skill.is_none_or(|want| want == name.as_str())
                    && filter_agent.is_none_or(|want| Some(want) == owner.as_deref());
                if selected {
                    matched_dirs.insert(dir, name.clone());
                }
                for tool in skill.tools.values() {
                    if selected && tool.server == alias {
                        removed_scopes.extend(tool.scopes.iter().cloned());
                        removed_secrets
                            .extend(tool.secrets.values().map(|s| s.as_str().to_owned()));
                    } else {
                        remaining_scopes.extend(tool.scopes.iter().cloned());
                        remaining_secrets
                            .extend(tool.secrets.values().map(|s| s.as_str().to_owned()));
                    }
                }
            }
        }
        if !in_host && matched_dirs.is_empty() {
            return Err(reject(
                "server_not_found",
                format!("MCP server `{alias}` not found"),
            ));
        }

        let mut still_used_elsewhere = false;
        for (pid, snap) in set.active().iter() {
            for agent in snap.agents.values() {
                for skill in agent.skills.values() {
                    let dir = skill
                        .instructions
                        .relative_path
                        .strip_suffix("/SKILL.md")
                        .unwrap_or(&skill.instructions.relative_path);
                    let is_removed_here = *pid == call.profile && matched_dirs.contains_key(dir);
                    if !is_removed_here && skill.tools.values().any(|t| t.server == alias) {
                        still_used_elsewhere = true;
                    }
                }
            }
        }

        let profile_dir = self.profile_dir(&call.profile);
        let mut txn = DiskTxn::new();
        let mut removed_skills = Vec::new();
        for (dir, name) in &matched_dirs {
            let full_dir = profile_dir.join(dir);
            if delete_skill {
                txn.remove_skill_dir(&full_dir)
                    .map_err(|m| reject("io_error", m))?;
                removed_skills.push(name.clone());
            } else {
                let tools_path = full_dir.join("tools.yaml");
                if tools_path.exists() {
                    txn.remove_file(&tools_path)
                        .map_err(|m| reject("io_error", m))?;
                }
                let lock_path = full_dir.join(lock::FILE);
                if lock_path.exists() {
                    txn.remove_file(&lock_path)
                        .map_err(|m| reject("io_error", m))?;
                }
            }
        }

        let mut removed_host_server = false;
        if in_host && !still_used_elsewhere {
            if let Some(bearer) = host_remove_server(&mut host_doc, alias) {
                removed_secrets.insert(bearer);
            }
            removed_host_server = true;
            let host_rendered = serde_json::to_string_pretty(&host_doc)
                .map_err(|_| reject("invalid_host", "failed to serialize host.json"))?;
            txn.write_file(&host_path, format!("{host_rendered}\n").as_bytes())
                .map_err(|m| reject("io_error", m))?;
        }

        if let Some(servers) = host_doc.get("servers").and_then(Value::as_object) {
            for s in servers.values() {
                if let Some(sec) = s
                    .get("auth")
                    .and_then(|a| a.get("secret"))
                    .and_then(Value::as_str)
                {
                    remaining_secrets.insert(sec.to_owned());
                }
            }
        }
        let drop_scopes: BTreeSet<String> = removed_scopes
            .difference(&remaining_scopes)
            .filter(|s| s.as_str() != "run" && !bot_core::builtin::reserved(s))
            .cloned()
            .collect();
        let drop_secrets: BTreeSet<String> = if remove_secrets {
            removed_secrets
                .difference(&remaining_secrets)
                .cloned()
                .collect()
        } else {
            BTreeSet::new()
        };
        if !drop_scopes.is_empty() || !drop_secrets.is_empty() {
            let profile_yaml_path = profile_dir.join("profile.yaml");
            let text = fs::read_to_string(&profile_yaml_path)
                .map_err(|_| reject("io_error", "failed to read profile.yaml"))?;
            let mut profile_doc =
                bot_profile::parse_yaml("profile.yaml", &text, &Limits::default())
                    .map_err(|e| reject("invalid_profile", e.to_string()))?;
            profile_prune(&mut profile_doc, &drop_scopes, &drop_secrets);
            txn.write_file(&profile_yaml_path, render_yaml(&profile_doc).as_bytes())
                .map_err(|m| reject("io_error", m))?;
        }

        let new_rev = self.activate_disk(&call.profile, txn).await?;
        let mut unset_secrets = Vec::new();
        for name in &drop_secrets {
            if let Some(parsed) = SecretName::parse(name)
                && self
                    .secrets_admin
                    .unset_secret(&call.profile, &parsed)
                    .await
                    .is_ok()
            {
                unset_secrets.push(name.clone());
            }
        }

        let payload = json!({
            "ok": true,
            "action": "mcp.remove",
            "alias": alias,
            "removed_host_server": removed_host_server,
            "removed_skills": removed_skills,
            "removed_secrets": unset_secrets,
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }
}
