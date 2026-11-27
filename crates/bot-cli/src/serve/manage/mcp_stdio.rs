//! `mcp.add`의 로컬 stdio MCP 서버 추가·경로 격리 검증·읽기 전용 설치·즉시 활성화.
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

/// 로컬 stdio MCP 서버의 `root`가 호스트 핵심 시스템·비밀·프로필·설치 폴더를 가리키지 않는지 막는다.
fn check_stdio_root(root: &Path, forbidden: &[PathBuf]) -> Result<PathBuf, ManageError> {
    if !root.is_absolute() || root == Path::new("/") {
        return Err(reject(
            "invalid_root",
            "root는 `/`가 아닌 절대 경로여야 해요",
        ));
    }
    let meta = fs::symlink_metadata(root).map_err(|_| {
        reject(
            "invalid_root",
            format!("root 폴더를 찾지 못했어요: {}", root.display()),
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(reject(
            "invalid_root",
            "root는 심볼릭 링크가 아닌 일반 폴더여야 해요",
        ));
    }
    let canon = fs::canonicalize(root)
        .map_err(|_| reject("invalid_root", "root 경로를 확인할 수 없어요"))?;
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
                    "시스템 폴더는 MCP 서버 root로 지정할 수 없어요: {}",
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
                "프로필·호스트 설정·비밀·설치·홈 폴더와 겹치는 경로는 MCP 서버 root로 쓸 수 없어요",
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
                    "로컬 stdio MCP에는 root 절대 경로가 필요해요",
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
                    "로컬 stdio MCP에는 비어 있지 않은 command 배열이 필요해요",
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
                    "로컬 stdio MCP에는 stdio_tools 정의가 하나 이상 필요해요",
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
                        reject(
                            "invalid_secret",
                            format!("비밀 이름이 올바르지 않아요: {raw_name}"),
                        )
                    })?;
                let val_str = val.as_str().ok_or_else(|| {
                    reject("invalid_secret", "secret_values 값은 문자열이어야 해요")
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
                .ok_or_else(|| reject("invalid_args", "stdio_tools 항목은 객체여야 해요"))?;
            let raw_name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| reject("invalid_args", "도구 name이 필요해요"))?;
            let id = sanitize_tool_id(raw_name, &mut used_ids);
            let class = obj
                .get("class")
                .and_then(Value::as_str)
                .ok_or_else(|| reject("invalid_args", "도구 class가 필요해요"))?;
            let input = obj
                .get("input")
                .filter(|v| v.is_object())
                .ok_or_else(|| reject("invalid_args", "도구 input JSON 스키마 객체가 필요해요"))?;
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
            .map_err(|_| reject("io_error", "profile.yaml을 읽지 못했어요"))?;
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
            .map_err(|_| reject("invalid_host", "host.json을 직렬화하지 못했어요"))?;
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
