//! Transactional disk edits (`DiskTxn`) and deterministic YAML, SKILL.md, and host.json helpers.
use bot_core::{canon::json_quote, id::is_name};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

enum Undo {
    RestoreFile {
        path: PathBuf,
        bytes: Vec<u8>,
    },
    RemoveFile {
        path: PathBuf,
    },
    RemoveDir {
        path: PathBuf,
    },
    RestoreDir {
        path: PathBuf,
        files: BTreeMap<PathBuf, Vec<u8>>,
    },
}

/// Rolls back recorded filesystem mutations in reverse order if dropped without `commit()`.
pub(super) struct DiskTxn {
    undo: Vec<Undo>,
    committed: bool,
}

impl DiskTxn {
    pub(super) fn new() -> Self {
        Self {
            undo: Vec::new(),
            committed: false,
        }
    }

    pub(super) fn commit(mut self) {
        self.committed = true;
    }

    pub(super) fn write_file(&mut self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(format!("cannot write non-regular file: {}", path.display()));
            }
            let prev = fs::read(path)
                .map_err(|e| format!("failed to read existing file ({}): {e}", path.display()))?;
            self.undo.push(Undo::RestoreFile {
                path: path.to_path_buf(),
                bytes: prev,
            });
        } else {
            if let Some(parent) = path.parent() {
                self.ensure_dir(parent)?;
            }
            self.undo.push(Undo::RemoveFile {
                path: path.to_path_buf(),
            });
        }
        fs::write(path, bytes)
            .map_err(|e| format!("failed to write file ({}): {e}", path.display()))
    }

    pub(super) fn ensure_dir(&mut self, dir: &Path) -> Result<(), String> {
        let mut missing = Vec::new();
        let mut cur = dir;
        while !cur.exists() {
            missing.push(cur.to_path_buf());
            let Some(parent) = cur.parent() else {
                break;
            };
            cur = parent;
        }
        if let Ok(meta) = fs::symlink_metadata(cur)
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            return Err(format!("path is not a directory: {}", cur.display()));
        }
        missing.reverse();
        for part in missing {
            fs::create_dir(&part)
                .map_err(|e| format!("failed to create directory ({}): {e}", part.display()))?;
            self.undo.push(Undo::RemoveDir { path: part });
        }
        Ok(())
    }

    pub(super) fn remove_file(&mut self, path: &Path) -> Result<(), String> {
        let meta = fs::symlink_metadata(path)
            .map_err(|e| format!("failed to stat file ({}): {e}", path.display()))?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(format!(
                "cannot remove non-regular file: {}",
                path.display()
            ));
        }
        let prev =
            fs::read(path).map_err(|e| format!("failed to read file ({}): {e}", path.display()))?;
        fs::remove_file(path)
            .map_err(|e| format!("failed to remove file ({}): {e}", path.display()))?;
        self.undo.push(Undo::RestoreFile {
            path: path.to_path_buf(),
            bytes: prev,
        });
        Ok(())
    }

    pub(super) fn remove_skill_dir(&mut self, dir: &Path) -> Result<(), String> {
        let meta = fs::symlink_metadata(dir)
            .map_err(|e| format!("failed to stat skill directory ({}): {e}", dir.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!("invalid skill directory: {}", dir.display()));
        }
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(dir)
            .map_err(|e| format!("failed to read skill directory ({}): {e}", dir.display()))?
        {
            let entry = entry.map_err(|e| format!("failed to read directory entry: {e}"))?;
            let path = entry.path();
            let fmeta = fs::symlink_metadata(&path)
                .map_err(|e| format!("failed to stat entry ({}): {e}", path.display()))?;
            if fmeta.file_type().is_symlink() || !fmeta.is_file() {
                return Err(format!(
                    "skill directory contains non-regular entry: {}",
                    path.display()
                ));
            }
            let bytes = fs::read(&path)
                .map_err(|e| format!("failed to read file ({}): {e}", path.display()))?;
            files.insert(path, bytes);
        }
        for path in files.keys() {
            fs::remove_file(path)
                .map_err(|e| format!("failed to remove file ({}): {e}", path.display()))?;
        }
        fs::remove_dir(dir)
            .map_err(|e| format!("failed to remove skill directory ({}): {e}", dir.display()))?;
        self.undo.push(Undo::RestoreDir {
            path: dir.to_path_buf(),
            files,
        });
        Ok(())
    }
}

impl Drop for DiskTxn {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for action in self.undo.drain(..).rev() {
            match action {
                Undo::RestoreFile { path, bytes } => {
                    if let Some(parent) = path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(path, bytes);
                }
                Undo::RemoveFile { path } => {
                    let _ = fs::remove_file(path);
                }
                Undo::RemoveDir { path } => {
                    let _ = fs::remove_dir(path);
                }
                Undo::RestoreDir { path, files } => {
                    let _ = fs::create_dir_all(&path);
                    for (file, bytes) in files {
                        let _ = fs::write(file, bytes);
                    }
                }
            }
        }
    }
}

pub(super) fn agent_rel_dir(agent: Option<&str>) -> Result<PathBuf, String> {
    let Some(agent) = agent else {
        return Ok(PathBuf::new());
    };
    let mut out = PathBuf::new();
    for segment in agent.split('/') {
        if !is_name(segment) {
            return Err(format!("invalid agent path: {agent}"));
        }
        out.push("agents");
        out.push(segment);
    }
    Ok(out)
}

pub(super) fn skill_rel_dir(agent: Option<&str>, name: &str) -> Result<PathBuf, String> {
    if !is_name(name) {
        return Err(format!(
            "skill name must start with a lowercase letter and use only lowercase letters, digits, and hyphens (max 63 chars): {name}"
        ));
    }
    Ok(agent_rel_dir(agent)?.join("skills").join(name))
}

pub(super) fn agent_of_skill_path(rel_path: &str) -> Option<String> {
    let prefix = rel_path.strip_suffix("/SKILL.md")?;
    let (head, _) = prefix.rsplit_once("/skills/")?;
    let parts: Vec<&str> = head.split('/').collect();
    if !parts.len().is_multiple_of(2) {
        return None;
    }
    let mut segments = Vec::new();
    for pair in parts.chunks(2) {
        if pair[0] != "agents" {
            return None;
        }
        segments.push(pair[1]);
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

/// Double-quoted YAML string escaping control and line-separator characters for strict YAML 1.2.
pub(super) fn yaml_quote(text: &str) -> String {
    let raw = json_quote(text);
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch as u32 {
            0x7f..=0x9f | 0x2028 | 0x2029 | 0xfeff | 0xfffe | 0xffff => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    let body = body.trim();
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n{body}\n",
        yaml_quote(name),
        yaml_quote(description.trim()),
    )
}

pub(super) fn render_yaml(value: &Value) -> String {
    let mut out = String::new();
    write_yaml(value, 0, &mut out);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn safe_yaml_key(key: &str) -> String {
    let plain = !key.is_empty()
        && !matches!(
            key,
            "true" | "false" | "null" | "True" | "False" | "Null" | "TRUE" | "FALSE" | "NULL" | "~"
        )
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
        && key.as_bytes()[0].is_ascii_alphabetic();
    if plain {
        key.to_owned()
    } else {
        yaml_quote(key)
    }
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn scalar_yaml(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => yaml_quote(s),
        _ => unreachable!(),
    }
}

fn write_yaml(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}\n");
                return;
            }
            for (k, v) in map {
                out.push_str(&pad);
                out.push_str(&safe_yaml_key(k));
                out.push(':');
                write_value_after_colon(v, indent, out);
            }
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]\n");
                return;
            }
            for item in items {
                out.push_str(&pad);
                out.push('-');
                write_array_item(item, indent, out);
            }
        }
        other => {
            out.push_str(&pad);
            out.push_str(&scalar_yaml(other));
            out.push('\n');
        }
    }
}

fn write_value_after_colon(v: &Value, indent: usize, out: &mut String) {
    match v {
        v if is_scalar(v) => {
            out.push(' ');
            out.push_str(&scalar_yaml(v));
            out.push('\n');
        }
        Value::Array(items) if items.is_empty() => out.push_str(" []\n"),
        Value::Object(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Array(items) if items.iter().all(is_scalar) && items.len() <= 6 => {
            let joined: Vec<String> = items.iter().map(scalar_yaml).collect();
            let line = format!(" [{}]\n", joined.join(", "));
            if line.len() <= 100 {
                out.push_str(&line);
            } else {
                out.push('\n');
                write_yaml(v, indent + 2, out);
            }
        }
        _ => {
            out.push('\n');
            write_yaml(v, indent + 2, out);
        }
    }
}

fn write_array_item(item: &Value, indent: usize, out: &mut String) {
    match item {
        v if is_scalar(v) => {
            out.push(' ');
            out.push_str(&scalar_yaml(v));
            out.push('\n');
        }
        Value::Object(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Array(arr) if arr.is_empty() => out.push_str(" []\n"),
        Value::Object(map) => {
            let mut first = true;
            let child_pad = " ".repeat(indent + 2);
            for (k, v) in map {
                if first {
                    out.push(' ');
                    first = false;
                } else {
                    out.push_str(&child_pad);
                }
                out.push_str(&safe_yaml_key(k));
                out.push(':');
                write_value_after_colon(v, indent + 2, out);
            }
        }
        _ => {
            out.push('\n');
            write_yaml(item, indent + 2, out);
        }
    }
}

fn push_unique_strings(arr: &mut Vec<Value>, add: &[String]) {
    let mut seen: BTreeSet<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for item in add {
        if seen.insert(item.clone()) {
            arr.push(Value::String(item.clone()));
        }
    }
}

fn remove_strings(arr: &mut Vec<Value>, drop_set: &BTreeSet<String>) {
    arr.retain(|v| !v.as_str().is_some_and(|s| drop_set.contains(s)));
}

pub(super) fn profile_add_scopes_and_secrets(
    doc: &mut Value,
    roles: &[String],
    agent: Option<&str>,
    root_agent: &str,
    scopes: &[String],
    secrets: &[String],
) -> Result<(), String> {
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| "profile.yaml must be a mapping".to_owned())?;
    if !scopes.is_empty() {
        let role_map = obj
            .get_mut("roles")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| "profile.yaml is missing `roles`".to_owned())?;
        for role in roles {
            let Some(list) = role_map.get_mut(role).and_then(Value::as_array_mut) else {
                return Err(format!("role `{role}` does not exist in profile.yaml"));
            };
            push_unique_strings(list, scopes);
        }
        if let Some(policy) = obj.get_mut("policy").and_then(Value::as_object_mut) {
            if let Some(cap) = policy.get_mut("scopes").and_then(Value::as_array_mut) {
                push_unique_strings(cap, scopes);
            }
            if let Some(agents) = policy.get_mut("agents").and_then(Value::as_object_mut) {
                let target = agent.unwrap_or(root_agent);
                let mut lineage = Vec::new();
                let mut acc = String::new();
                for part in target.split('/') {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    lineage.push(acc.clone());
                }
                for ancestor in lineage {
                    if let Some(cap) = agents.get_mut(&ancestor).and_then(Value::as_array_mut) {
                        push_unique_strings(cap, scopes);
                    }
                }
            }
        }
    }
    if !secrets.is_empty() {
        let list = obj
            .entry("secrets")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| "`secrets` in profile.yaml must be an array".to_owned())?;
        push_unique_strings(list, secrets);
    }
    Ok(())
}

pub(super) fn profile_prune(
    doc: &mut Value,
    drop_scopes: &BTreeSet<String>,
    drop_secrets: &BTreeSet<String>,
) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    if !drop_scopes.is_empty() {
        if let Some(roles) = obj.get_mut("roles").and_then(Value::as_object_mut) {
            for list in roles.values_mut().filter_map(Value::as_array_mut) {
                remove_strings(list, drop_scopes);
            }
        }
        if let Some(policy) = obj.get_mut("policy").and_then(Value::as_object_mut) {
            if let Some(list) = policy.get_mut("scopes").and_then(Value::as_array_mut) {
                remove_strings(list, drop_scopes);
            }
            if let Some(list) = policy.get_mut("approve").and_then(Value::as_array_mut) {
                remove_strings(list, drop_scopes);
            }
            if let Some(agents) = policy.get_mut("agents").and_then(Value::as_object_mut) {
                for list in agents.values_mut().filter_map(Value::as_array_mut) {
                    remove_strings(list, drop_scopes);
                }
            }
        }
    }
    if !drop_secrets.is_empty()
        && let Some(list) = obj.get_mut("secrets").and_then(Value::as_array_mut)
    {
        remove_strings(list, drop_secrets);
    }
}

pub(super) fn host_add_server(
    doc: &mut Value,
    alias: &str,
    entry: Value,
    profile: &str,
    egress_targets: &[String],
) -> Result<(), String> {
    let top = doc
        .as_object_mut()
        .ok_or_else(|| "host.json must be a JSON object".to_owned())?;
    let servers = top
        .entry("servers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| "`servers` in host.json must be an object".to_owned())?;
    if servers.contains_key(alias) {
        return Err(format!("server `{alias}` already exists in host config"));
    }
    servers.insert(alias.to_owned(), entry);
    if !egress_targets.is_empty()
        && let Some(egress) = top.get_mut("egress").and_then(Value::as_object_mut)
        && let Some(list) = egress.get_mut(profile).and_then(Value::as_array_mut)
    {
        push_unique_strings(list, egress_targets);
    }
    Ok(())
}

pub(super) fn host_remove_server(doc: &mut Value, alias: &str) -> Option<String> {
    let servers = doc.as_object_mut()?.get_mut("servers")?.as_object_mut()?;
    let removed = servers.remove(alias)?;
    removed
        .get("auth")
        .and_then(|a| a.get("secret"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_profile::Limits;
    use serde_json::json;

    #[test]
    fn yaml_renderer_round_trips_through_strict_profile_parser() {
        let doc = json!({
            "version": 1,
            "id": "team",
            "model": "assistant",
            "super_admins": ["local:owner", "slack:home/U1"],
            "roles": {"owner": ["run", "skill.create", "true"]},
            "members": [{"identity": "local:owner", "roles": ["owner"]}],
            "bindings": [{"id": "desk", "adapter": "cli", "account": "owner", "agent": "main"}],
            "secrets": [],
            "policy": {"scopes": ["run"], "agents": {"main/developer": ["run"]}, "approve": []}
        });
        let text = render_yaml(&doc);
        let back = bot_profile::parse_yaml("profile.yaml", &text, &Limits::default()).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn disk_txn_rolls_back_all_mutations_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("keep.txt");
        fs::write(&existing, "before").unwrap();
        let created = dir.path().join("sub/new.txt");
        {
            let mut txn = DiskTxn::new();
            txn.write_file(&existing, b"after").unwrap();
            txn.write_file(&created, b"hello").unwrap();
            assert_eq!(fs::read_to_string(&existing).unwrap(), "after");
            assert!(created.exists());
        }
        assert_eq!(fs::read_to_string(&existing).unwrap(), "before");
        assert!(!created.exists());
        assert!(!dir.path().join("sub").exists());
    }
}
