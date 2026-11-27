//! Live `skill.create`, `skill.update`, and `skill.delete` handlers with immediate activation.
use super::{
    LiveManager,
    yaml_edit::{
        DiskTxn, agent_of_skill_path, profile_prune, render_skill_md, render_yaml, skill_rel_dir,
    },
};
use bot_broker::guard::blocks_output;
use bot_core::{
    command::JsonPayload,
    id::AgentId,
    manage::{ManageCall, ManageError, ManageOutcome},
    profile::Snapshot,
};
use bot_profile::Limits;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn reject(code: &'static str, message: impl Into<String>) -> ManageError {
    ManageError::Rejected {
        code,
        message: message.into(),
    }
}

fn check_skill_text(description: Option<&str>, body: Option<&str>) -> Result<(), ManageError> {
    if let Some(desc) = description {
        let trimmed = desc.trim();
        if trimmed.is_empty() || trimmed.len() > 1024 {
            return Err(reject(
                "invalid_description",
                "description must be 1..=1024 bytes",
            ));
        }
        if blocks_output(trimmed, &[]) {
            return Err(reject(
                "secret_in_skill",
                "description contains a secret-like token or key — store secrets in SOPS/keychain and reference them by name",
            ));
        }
    }
    if let Some(text) = body {
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.len() > bot_profile::SKILL_MAX_BYTES {
            return Err(reject(
                "invalid_body",
                "skill body must be between 1 byte and 64 KiB",
            ));
        }
        if trimmed.starts_with("---\n") || trimmed.starts_with("---\r\n") {
            return Err(reject(
                "body_has_frontmatter",
                "pass markdown body only without YAML front matter (`---`); name and description are separate arguments",
            ));
        }
        if blocks_output(trimmed, &[]) {
            return Err(reject(
                "secret_in_skill",
                "skill body contains a secret-like token or key — store secrets in SOPS/keychain and reference them by name",
            ));
        }
    }
    Ok(())
}

fn locate_existing_skill(
    snapshot: &Snapshot,
    name: &str,
    explicit_agent: Option<&str>,
) -> Result<(Option<String>, String), ManageError> {
    let mut matches: Vec<(Option<String>, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for agent in snapshot.agents.values() {
        if let Some(skill) = agent.skills.get(name) {
            let rel = skill.instructions.relative_path.clone();
            if seen.insert(rel.clone()) {
                let owner = agent_of_skill_path(&rel);
                if explicit_agent.is_none() || explicit_agent == owner.as_deref() {
                    let dir = rel.strip_suffix("/SKILL.md").unwrap_or(&rel).to_owned();
                    matches.push((owner, dir));
                }
            }
        }
    }
    match matches.as_slice() {
        [] => Err(reject(
            "skill_not_found",
            format!("skill `{name}` not found"),
        )),
        [one] => Ok(one.clone()),
        _ => {
            let locs: Vec<String> = matches
                .iter()
                .map(|(a, _)| a.clone().unwrap_or_else(|| "profile-shared".into()))
                .collect();
            Err(reject(
                "ambiguous_skill",
                format!(
                    "skill `{name}` exists in multiple scopes ({}) — specify `agent`",
                    locs.join(", ")
                ),
            ))
        }
    }
}

fn clean_profile_scopes_after_delete(
    profile_dir: &Path,
    snapshot: &Snapshot,
    deleted_rel_dir: &str,
    txn: &mut DiskTxn,
) -> Result<(), ManageError> {
    let deleted_prefix = format!("{deleted_rel_dir}/");
    let mut removed_scopes = BTreeSet::new();
    let mut remaining_scopes = BTreeSet::new();
    for agent in snapshot.agents.values() {
        for skill in agent.skills.values() {
            let is_deleted = skill
                .instructions
                .relative_path
                .starts_with(&deleted_prefix);
            for tool in skill.tools.values() {
                if is_deleted {
                    removed_scopes.extend(tool.scopes.iter().cloned());
                } else {
                    remaining_scopes.extend(tool.scopes.iter().cloned());
                }
            }
        }
    }
    let drop_scopes: BTreeSet<String> = removed_scopes
        .difference(&remaining_scopes)
        .filter(|s| s.as_str() != "run" && !bot_core::builtin::reserved(s))
        .cloned()
        .collect();
    if drop_scopes.is_empty() {
        return Ok(());
    }
    let profile_yaml_path = profile_dir.join("profile.yaml");
    let text = fs::read_to_string(&profile_yaml_path)
        .map_err(|_| reject("io_error", "failed to read profile.yaml"))?;
    let mut doc = bot_profile::parse_yaml("profile.yaml", &text, &Limits::default())
        .map_err(|e| reject("invalid_profile", e.to_string()))?;
    profile_prune(&mut doc, &drop_scopes, &BTreeSet::new());
    let rendered = render_yaml(&doc);
    txn.write_file(&profile_yaml_path, rendered.as_bytes())
        .map_err(|m| reject("io_error", m))
}

impl LiveManager {
    pub(super) async fn create_skill(
        &self,
        call: &ManageCall,
    ) -> Result<ManageOutcome, ManageError> {
        let args: Value = serde_json::from_str(&call.args.0)
            .map_err(|_| reject("invalid_args", "failed to parse request arguments"))?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "name is required"))?;
        let agent = args.get("agent").and_then(Value::as_str);
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "description is required"))?;
        let body = args
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "body is required"))?;
        check_skill_text(Some(description), Some(body))?;

        let _guard = self.tx_lock.lock().await;
        let snapshot = self.current_snapshot(&call.profile)?;
        if let Some(target) = agent
            && !snapshot.agents.contains_key(&AgentId(target.to_owned()))
        {
            return Err(reject(
                "unknown_agent",
                format!("agent `{target}` does not exist in this profile"),
            ));
        }
        let rel_dir = skill_rel_dir(agent, name).map_err(|m| reject("invalid_name", m))?;
        let profile_dir = self.profile_dir(&call.profile);
        let skill_dir = profile_dir.join(&rel_dir);
        if skill_dir.exists() {
            return Err(reject(
                "skill_exists",
                format!(
                    "skill `{name}` already exists ({}) — use `skill.update` to modify it",
                    rel_dir.display()
                ),
            ));
        }
        let mut txn = DiskTxn::new();
        let content = render_skill_md(name, description, body);
        txn.write_file(&skill_dir.join("SKILL.md"), content.as_bytes())
            .map_err(|m| reject("io_error", m))?;
        let new_rev = self.activate_disk(&call.profile, txn).await?;
        let payload = json!({
            "ok": true,
            "action": "skill.create",
            "profile": call.profile.0,
            "skill": name,
            "agent": agent,
            "path": rel_dir.display().to_string(),
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }

    pub(super) async fn update_skill(
        &self,
        call: &ManageCall,
    ) -> Result<ManageOutcome, ManageError> {
        let args: Value = serde_json::from_str(&call.args.0)
            .map_err(|_| reject("invalid_args", "failed to parse request arguments"))?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "name is required"))?;
        let explicit_agent = args.get("agent").and_then(Value::as_str);
        let new_desc = args.get("description").and_then(Value::as_str);
        let new_body = args.get("body").and_then(Value::as_str);
        if new_desc.is_none() && new_body.is_none() {
            return Err(reject(
                "invalid_args",
                "at least one of description or body is required",
            ));
        }
        check_skill_text(new_desc, new_body)?;

        let _guard = self.tx_lock.lock().await;
        let snapshot = self.current_snapshot(&call.profile)?;
        let (owner, rel_dir) = locate_existing_skill(&snapshot, name, explicit_agent)?;
        let current_skill = snapshot
            .agents
            .values()
            .find_map(|a| {
                a.skills
                    .get(name)
                    .filter(|s| s.instructions.relative_path == format!("{rel_dir}/SKILL.md"))
            })
            .ok_or_else(|| reject("skill_not_found", format!("skill `{name}` not found")))?;

        let desc = new_desc
            .map(str::to_owned)
            .or_else(|| current_skill.description.clone())
            .unwrap_or_else(|| format!("{name} skill"));
        let body = new_body.unwrap_or(&current_skill.instructions.text);
        let content = render_skill_md(name, &desc, body);

        let skill_md = self
            .profile_dir(&call.profile)
            .join(&rel_dir)
            .join("SKILL.md");
        let mut txn = DiskTxn::new();
        txn.write_file(&skill_md, content.as_bytes())
            .map_err(|m| reject("io_error", m))?;
        let new_rev = self.activate_disk(&call.profile, txn).await?;
        let payload = json!({
            "ok": true,
            "action": "skill.update",
            "profile": call.profile.0,
            "skill": name,
            "agent": owner,
            "path": rel_dir,
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }

    pub(super) async fn delete_skill(
        &self,
        call: &ManageCall,
    ) -> Result<ManageOutcome, ManageError> {
        let args: Value = serde_json::from_str(&call.args.0)
            .map_err(|_| reject("invalid_args", "failed to parse request arguments"))?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("invalid_args", "name is required"))?;
        let explicit_agent = args.get("agent").and_then(Value::as_str);

        let _guard = self.tx_lock.lock().await;
        let snapshot = self.current_snapshot(&call.profile)?;
        let (owner, rel_dir) = locate_existing_skill(&snapshot, name, explicit_agent)?;
        let profile_dir = self.profile_dir(&call.profile);
        let skill_dir: PathBuf = profile_dir.join(&rel_dir);

        let mut txn = DiskTxn::new();
        txn.remove_skill_dir(&skill_dir)
            .map_err(|m| reject("io_error", m))?;
        clean_profile_scopes_after_delete(&profile_dir, &snapshot, &rel_dir, &mut txn)?;
        let new_rev = self.activate_disk(&call.profile, txn).await?;
        let payload = json!({
            "ok": true,
            "action": "skill.delete",
            "profile": call.profile.0,
            "skill": name,
            "agent": owner,
            "path": rel_dir,
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }
}
