//! Local stdio MCP server registration (`mcp.add`), root-isolation checks, and live activation.
use super::{
    LiveManager,
    mcp::{AddCtx, auto_mcp_skill_md, reject, sanitize_tool_id},
    yaml_edit::{DiskTxn, host_add_server, profile_add_scopes_and_secrets, render_yaml},
};
use bot_core::{
    command::JsonPayload,
    id::SecretName,
    manage::{ManageError, ManageOutcome},
};
use bot_profile::Limits;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// Rejects system, profile, host-config, secret, install, and home paths as local stdio MCP roots.
fn check_stdio_root(root: &Path, forbidden: &[PathBuf]) -> Result<PathBuf, ManageError> {
    if !root.is_absolute() || root == Path::new("/") {
        return Err(reject(
            "invalid_root",
            "root must be an absolute path other than `/`",
        ));
    }
    let meta = fs::symlink_metadata(root).map_err(|_| {
        reject(
            "invalid_root",
            format!("root directory not found: {}", root.display()),
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(reject(
            "invalid_root",
            "root must be a regular directory, not a symlink",
        ));
    }
    let canon = fs::canonicalize(root)
        .map_err(|_| reject("invalid_root", "failed to resolve root path"))?;
    const SYSTEM: &[&str] = &[
        "/",
        "/bin",
        "/sbin",
        "/usr",
        "/etc",
        "/var",
        "/dev",
        "/proc",
        "/sys",
        "/System",
        "/Library",
        "/private/etc",
    ];
    for blocked in SYSTEM {
        let b = Path::new(blocked);
        if canon == b || (*blocked != "/" && canon.starts_with(b)) {
            return Err(reject(
                "forbidden_root",
                format!(
                    "system directory cannot be used as MCP server root: {}",
                    canon.display()
                ),
            ));
        }
    }
    for dir in forbidden {
        if let Ok(fb) = fs::canonicalize(dir)
            && (canon == fb || canon.starts_with(&fb) || fb.starts_with(&canon))
        {
            return Err(reject(
                "forbidden_root",
                "MCP server root cannot overlap profile, host config, secret, install, or home directories",
            ));
        }
    }
    Ok(canon)
}

impl LiveManager {
    pub(super) async fn add_stdio_mcp(
        &self,
        ctx: &AddCtx<'_>,
        host_doc: &mut Value,
        created_secrets: &mut Vec<SecretName>,
    ) -> Result<ManageOutcome, ManageError> {
        let root_str = ctx
            .args
            .get("root")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                reject(
                    "invalid_args",
                    "local stdio MCP requires an absolute `root` path",
                )
            })?;
        let command: Vec<String> = ctx
            .args
            .get("command")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_owned)).collect())
            .ok_or_else(|| {
                reject(
                    "invalid_args",
                    "local stdio MCP requires a non-empty `command` array",
                )
            })?;
        let stdio_tools = ctx
            .args
            .get("stdio_tools")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                reject(
                    "invalid_args",
                    "local stdio MCP requires at least one `stdio_tools` entry",
                )
            })?;

        let mut forbidden = vec![self.profiles_root.clone()];
        if let Some(parent) = ctx.host_path.parent() {
            forbidden.push(parent.to_path_buf());
        }
        if let Some(home) = std::env::var_os("HOME") {
            forbidden.push(PathBuf::from(home));
        }
        for p in self.settings.secrets.protected_paths() {
            if let Some(parent) = p.parent() {
                forbidden.push(parent.to_path_buf());
            }
        }
        if let Ok(base) = crate::install::default_base(
            self.settings.installs.as_deref(),
            std::env::var("HOME").ok().as_deref(),
        ) {
            forbidden.push(base);
        }
        let canon_root = check_stdio_root(Path::new(root_str), &forbidden)?;

        if let Some(secret_vals) = ctx.args.get("secret_values").and_then(Value::as_object) {
            for (raw_name, val) in secret_vals {
                let name = SecretName::parse(raw_name)
                    .filter(|n| !bot_broker::private::reserved_secret(n))
                    .ok_or_else(|| {
                        reject("invalid_secret", format!("invalid secret name: {raw_name}"))
                    })?;
                let val_str = val.as_str().ok_or_else(|| {
                    reject("invalid_secret", "secret_values entries must be strings")
                })?;
                self.store_secret_value(&ctx.call.profile, &name, val_str, created_secrets)
                    .await?;
            }
        }

        let mut tools_list = Vec::new();
        let mut new_scopes = Vec::new();
        let mut new_secrets = BTreeSet::new();
        let mut all_egress = BTreeSet::new();
        let mut summary_tools = Vec::new();
        let mut used_ids = BTreeSet::new();
        for item in stdio_tools {
            let obj = item
                .as_object()
                .ok_or_else(|| reject("invalid_args", "each stdio_tools item must be an object"))?;
            let raw_name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| reject("invalid_args", "tool `name` is required"))?;
            let id = sanitize_tool_id(raw_name, &mut used_ids);
            let class = obj
                .get("class")
                .and_then(Value::as_str)
                .ok_or_else(|| reject("invalid_args", "tool `class` is required"))?;
            let input = obj.get("input").filter(|v| v.is_object()).ok_or_else(|| {
                reject(
                    "invalid_args",
                    "tool `input` JSON Schema object is required",
                )
            })?;
            let scope = format!("{}.{id}", ctx.skill);
            new_scopes.push(scope.clone());
            summary_tools.push((
                id.clone(),
                obj.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ));
            let mut decl = Map::new();
            decl.insert("id".into(), json!(id));
            if raw_name != id {
                decl.insert("name".into(), json!(raw_name));
            }
            decl.insert("class".into(), json!(class));
            decl.insert("scopes".into(), json!([scope]));
            decl.insert("input".into(), input.clone());
            if let Some(egress) = obj.get("egress").and_then(Value::as_array) {
                let targets: Vec<String> = egress
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if !targets.is_empty() {
                    all_egress.extend(targets.iter().cloned());
                    decl.insert("egress".into(), json!(targets));
                }
            }
            if let Some(secrets) = obj.get("secrets").and_then(Value::as_object)
                && !secrets.is_empty()
            {
                for v in secrets.values().filter_map(Value::as_str) {
                    new_secrets.insert(v.to_owned());
                }
                decl.insert("secrets".into(), Value::Object(secrets.clone()));
            }
            if let Some(env) = obj.get("env").and_then(Value::as_array)
                && !env.is_empty()
            {
                decl.insert("env".into(), Value::Array(env.clone()));
            }
            if let Some(timeout) = obj.get("timeout_secs").and_then(Value::as_u64) {
                decl.insert("timeout_secs".into(), json!(timeout));
            }
            tools_list.push(Value::Object(decl));
        }

        let tools_doc = json!({
            "version": 1,
            "server": ctx.alias,
            "tools": tools_list,
        });
        let server_entry = json!({
            "root": canon_root.display().to_string(),
            "command": command,
        });
        let egress_vec: Vec<String> = all_egress.into_iter().collect();
        host_add_server(
            host_doc,
            ctx.alias,
            server_entry,
            &ctx.call.profile.0,
            &egress_vec,
        )
        .map_err(|m| reject("invalid_host", m))?;

        let profile_dir = self.profile_dir(&ctx.call.profile);
        let profile_yaml_path = profile_dir.join("profile.yaml");
        let profile_text = fs::read_to_string(&profile_yaml_path)
            .map_err(|_| reject("io_error", "failed to read profile.yaml"))?;
        let mut profile_doc =
            bot_profile::parse_yaml("profile.yaml", &profile_text, &Limits::default())
                .map_err(|e| reject("invalid_profile", e.to_string()))?;
        let secrets_vec: Vec<String> = new_secrets.into_iter().collect();
        profile_add_scopes_and_secrets(
            &mut profile_doc,
            ctx.roles,
            ctx.agent,
            &ctx.snapshot.root.0,
            &new_scopes,
            &secrets_vec,
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
        txn.write_file(
            &skill_dir.join("tools.yaml"),
            render_yaml(&tools_doc).as_bytes(),
        )
        .map_err(|m| reject("io_error", m))?;
        txn.write_file(&profile_yaml_path, render_yaml(&profile_doc).as_bytes())
            .map_err(|m| reject("io_error", m))?;
        let host_rendered = serde_json::to_string_pretty(host_doc)
            .map_err(|_| reject("invalid_host", "failed to serialize host.json"))?;
        txn.write_file(ctx.host_path, format!("{host_rendered}\n").as_bytes())
            .map_err(|m| reject("io_error", m))?;

        let new_rev = self.activate_disk(&ctx.call.profile, txn).await?;
        let tool_ids: Vec<String> = summary_tools
            .iter()
            .map(|(id, _)| format!("{}.{id}", ctx.skill))
            .collect();
        let payload = json!({
            "ok": true,
            "action": "mcp.add",
            "transport": "stdio",
            "alias": ctx.alias,
            "skill": ctx.skill,
            "agent": ctx.agent,
            "tools": tool_ids,
            "revision": new_rev.0,
            "active": true,
        });
        Ok(ManageOutcome {
            payload: JsonPayload(payload.to_string()),
            activated: Some(new_rev),
        })
    }
}
