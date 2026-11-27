//! Scaffolds or appends tool declarations in a skill's `tools.yaml` (`bot add tool`).
use super::{
    add::{NAME_RULE, agent_dir, finish, has_agent, lineage, open},
    files::Writer,
};
use crate::cli::ToolClass;
use bot_core::{builtin, id::is_name};
use bot_profile::{
    Limits,
    locate::{ToolsDoc, parse_tools_yaml},
};
use serde_json::{Value, json};
use std::path::Path;

/// Quote YAML 1.1/1.2 boolean/null literals so they parse as strings.
fn yaml_name(text: &str) -> String {
    if matches!(
        text,
        "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n" | "~"
    ) {
        format!("\"{text}\"")
    } else {
        text.to_owned()
    }
}

fn class_label(class: ToolClass) -> &'static str {
    match class {
        ToolClass::Read => "read",
        ToolClass::Write => "write",
        ToolClass::Irreversible => "irreversible",
    }
}

fn parse_tool_id(id: &str) -> Result<(&str, &str), String> {
    let Some((skill, tool)) = id.split_once('.') else {
        return Err(format!(
            "tool ID {id:?} must be `<skill>.<tool>` (both {NAME_RULE}) — e.g. agenda.search"
        ));
    };
    if !is_name(skill) {
        return Err(format!(
            "skill name {skill:?} does not match naming rule ({NAME_RULE}) — e.g. agenda.search"
        ));
    }
    if !is_name(tool) {
        return Err(format!(
            "tool name {tool:?} does not match naming rule ({NAME_RULE}) — e.g. agenda.search"
        ));
    }
    if builtin::reserved(id) {
        return Err(format!(
            "`{id}` is a reserved built-in tool name — choose a different tool name"
        ));
    }
    Ok((skill, tool))
}

struct SkillLocation {
    agent: Option<Vec<String>>,
    parts: Vec<String>,
}

impl SkillLocation {
    fn dir_path(&self) -> String {
        self.parts.join("/")
    }

    fn agent_path(&self) -> Option<String> {
        self.agent.as_ref().map(|a| a.join("/"))
    }
}

fn find_skill(writer: &Writer, skill: &str, agent: Option<&str>) -> Result<SkillLocation, String> {
    if let Some(agent_path) = agent {
        let scope = lineage(agent_path)?;
        if !has_agent(writer, &scope)? {
            return Err(format!(
                "agent {agent_path} does not exist — create it first with `bot add agent {agent_path}`"
            ));
        }
        let mut parts = agent_dir(&scope);
        parts.extend(["skills", skill]);
        let mut skill_md = parts.clone();
        skill_md.push("SKILL.md");
        if !writer.file_exists(&skill_md)? {
            return Err(format!(
                "skill `{skill}` does not exist under agent {agent_path} — create it first with `bot add skill {skill} --agent {agent_path}`"
            ));
        }
        return Ok(SkillLocation {
            agent: Some(scope.into_iter().map(str::to_owned).collect()),
            parts: parts.into_iter().map(str::to_owned).collect(),
        });
    }

    let mut found = Vec::new();
    if writer.file_exists(&["skills", skill, "SKILL.md"])? {
        found.push(SkillLocation {
            agent: None,
            parts: vec!["skills".to_owned(), skill.to_owned()],
        });
    }
    let limits = Limits::default();
    let mut budget = limits.max_dirs;
    let mut stack: Vec<Vec<String>> = writer
        .child_dirs(&["agents"])?
        .into_iter()
        .map(|root| vec![root])
        .collect();
    while let Some(path) = stack.pop() {
        let Some(left) = budget.checked_sub(1) else {
            break;
        };
        budget = left;
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        let here = agent_dir(&names);
        let mut skill_md = here.clone();
        skill_md.extend(["skills", skill, "SKILL.md"]);
        if writer.file_exists(&skill_md)? {
            let mut dir_parts: Vec<String> = here.iter().map(|p| (*p).to_owned()).collect();
            dir_parts.push("skills".to_owned());
            dir_parts.push(skill.to_owned());
            found.push(SkillLocation {
                agent: Some(path.clone()),
                parts: dir_parts,
            });
        }
        if path.len() >= limits.max_agent_depth {
            continue;
        }
        let mut children = here;
        children.push("agents");
        for child in writer.child_dirs(&children)? {
            let mut next = path.clone();
            next.push(child);
            stack.push(next);
        }
    }
    found.sort_by_key(SkillLocation::dir_path);
    match found.len() {
        0 => Err(format!(
            "skill `{skill}` not found — create it first with `bot add skill {skill}` (or `--agent <agent-path>`)"
        )),
        1 => Ok(found.remove(0)),
        _ => {
            let places: Vec<String> = found
                .iter()
                .map(|loc| match loc.agent_path() {
                    Some(a) => format!("`--agent {a}` ({})", loc.dir_path()),
                    None => loc.dir_path(),
                })
                .collect();
            Err(format!(
                "skill `{skill}` exists in multiple locations ({}) — specify `--agent <agent-path>`",
                places.join(", ")
            ))
        }
    }
}

fn new_tool_value(tool: &str, class: ToolClass, scope: &str) -> Value {
    json!({
        "id": tool,
        "class": class_label(class),
        "scopes": [scope],
        "secrets": {},
        "egress": [],
        "input": {"type": "object", "properties": {}, "additionalProperties": false}
    })
}

fn format_entry(indent: usize, tool: &str, class: ToolClass, scope: &str, eol: &str) -> String {
    let pad = " ".repeat(indent);
    let inner = " ".repeat(indent + 2);
    format!(
        "{pad}- id: {}{eol}\
         {inner}class: {}{eol}\
         {inner}scopes: [{scope}]{eol}\
         {inner}secrets: {{}}{eol}\
         {inner}egress: []{eol}\
         {inner}input: {{type: object, properties: {{}}, additionalProperties: false}}{eol}",
        yaml_name(tool),
        class_label(class),
    )
}

fn new_tools_yaml(server: &str, tool: &str, class: ToolClass, scope: &str) -> String {
    format!(
        "version: 1\nserver: {}\ntools:\n{}",
        yaml_name(server),
        format_entry(2, tool, class, scope, "\n")
    )
}

fn strip_eol(line: &str) -> (&str, &str) {
    if let Some(raw) = line.strip_suffix("\r\n") {
        (raw, "\r\n")
    } else if let Some(raw) = line.strip_suffix('\n') {
        (raw, "\n")
    } else {
        (line, "")
    }
}

fn strip_tools_key(raw: &str) -> Option<(&str, &str)> {
    for prefix in ["tools:", "\"tools\":", "'tools':"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return Some((prefix, rest));
        }
    }
    None
}

fn strip_empty_flow_list(after_key: &str) -> Option<&str> {
    let rest = after_key.trim_start_matches([' ', '\t']);
    let rest = rest.strip_prefix('[')?;
    let rest = rest.trim_start_matches([' ', '\t']);
    let tail = rest.strip_prefix(']')?;
    let check = tail.trim_start_matches([' ', '\t']);
    (check.is_empty() || check.starts_with('#')).then_some(tail)
}

fn is_seq_item(trimmed_start: &str) -> bool {
    trimmed_start == "-" || trimmed_start.starts_with("- ") || trimmed_start.starts_with("-\t")
}

/// Inserts a new tool entry while preserving existing comments, line endings, key order, and indentation.
/// Returns `None` without modifying the file when comment-preserving insertion is not possible (e.g. non-empty flow arrays).
fn append_preserving_comments(
    existing_text: &str,
    tool: &str,
    class: ToolClass,
    scope: &str,
) -> Option<String> {
    let eol = if existing_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines: Vec<&str> = existing_text.split_inclusive('\n').collect();
    let (tools_idx, key_prefix, after_key, tools_eol) =
        lines.iter().enumerate().find_map(|(idx, line)| {
            let (raw, line_eol) = strip_eol(line);
            let (prefix, rest) = strip_tools_key(raw)?;
            Some((idx, prefix, rest, line_eol))
        })?;

    if let Some(tail) = strip_empty_flow_list(after_key) {
        let mut out = String::with_capacity(existing_text.len() + 192);
        for line in &lines[..tools_idx] {
            out.push_str(line);
        }
        out.push_str(key_prefix);
        out.push_str(tail);
        out.push_str(if tools_eol.is_empty() { eol } else { tools_eol });
        out.push_str(&format_entry(2, tool, class, scope, eol));
        for line in &lines[tools_idx + 1..] {
            out.push_str(line);
        }
        return Some(out);
    }

    let after_trim = after_key.trim_start_matches([' ', '\t']);
    if !after_trim.is_empty() && !after_trim.starts_with('#') {
        return None;
    }

    let mut last_content_idx = tools_idx;
    let mut list_indent = None;
    for (idx, line) in lines.iter().enumerate().skip(tools_idx + 1) {
        let (raw, _) = strip_eol(line);
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if raw.starts_with(' ') || raw.starts_with('\t') {
            let spaces = raw.bytes().take_while(|b| *b == b' ').count();
            if list_indent.is_none() && is_seq_item(&raw[spaces..]) {
                list_indent = Some(spaces);
            }
            last_content_idx = idx;
            continue;
        }
        if is_seq_item(raw) {
            if list_indent.is_none() {
                list_indent = Some(0);
            }
            last_content_idx = idx;
            continue;
        }
        break;
    }

    let indent = list_indent.unwrap_or(2);
    let mut out = String::with_capacity(existing_text.len() + 192);
    for line in &lines[..=last_content_idx] {
        out.push_str(line);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(&format_entry(indent, tool, class, scope, eol));
    for line in &lines[last_content_idx + 1..] {
        out.push_str(line);
    }
    Some(out)
}

fn matches_appended(existing: &ToolsDoc, candidate: &ToolsDoc, expected_new: &Value) -> bool {
    if candidate.server != existing.server {
        return false;
    }
    let Some(old_list) = existing.value.get("tools").and_then(Value::as_array) else {
        return false;
    };
    let Some(new_list) = candidate.value.get("tools").and_then(Value::as_array) else {
        return false;
    };
    new_list.len() == old_list.len() + 1
        && new_list[..old_list.len()] == old_list[..]
        && &new_list[old_list.len()] == expected_new
}

fn format_issues(issues: &[bot_profile::Issue]) -> String {
    issues
        .iter()
        .map(|i| format!("  - {}: {} [{}]", i.at, i.message, i.code))
        .collect::<Vec<_>>()
        .join("\n")
}

fn next_steps(
    created: bool,
    rel: &str,
    skill_dir: &str,
    full_id: &str,
    class: ToolClass,
    server: &str,
    agent_path: Option<&str>,
) -> String {
    let verb = if created {
        format!(
            "Created: {rel} (tool {full_id}, class {})",
            class_label(class)
        )
    } else {
        format!(
            "Updated: added tool {full_id} (class {}) to {rel}",
            class_label(class)
        )
    };
    let mut policy_lines = vec![format!("    scopes: [..., {full_id}]")];
    if let Some(agent) = agent_path.filter(|a| a.contains('/')) {
        policy_lines.push("    agents:".to_owned());
        policy_lines.push(format!(
            "      {agent}: [..., {full_id}]   # if using policy.agents"
        ));
    }
    let approval_note = match class {
        ToolClass::Read => String::new(),
        ToolClass::Write => {
            policy_lines.push(format!(
                "    approve: [..., {full_id}]   # add to require human approval for this write tool"
            ));
            String::new()
        }
        ToolClass::Irreversible => {
            "\n  # Tools with `class: irreversible` always require human approval even without `policy.approve`."
                .to_owned()
        }
    };
    format!(
        "{verb}\n\
Permissions and host config were not modified automatically. Complete these four steps:\n\
\n\
1. Register the tool server under `servers` in host.json (local stdio example; see docs/plugins.md for remote HTTP):\n\
  \"servers\": {{\n\
    \"{server}\": {{\n\
      \"root\": \"/absolute/path/servers/{server}\",\n\
      \"command\": [\"bin/server\", \"--stdio\"]\n\
    }}\n\
  }}\n\
\n\
2. Allow the scope only on the needed roles and policy ceiling in profile.yaml:\n\
  roles:\n\
    owner: [..., {full_id}]\n\
  policy:\n\
{policy}{approval_note}\n\
\n\
3. (Optional) If the tool needs config, secrets, egress, or remote schema locking:\n\
  - Config: add keys to the agent's `env.yaml` and list them in `env: [KEY]` in tools.yaml.\n\
  - Secrets: declare `secrets: [my-secret]` in profile.yaml, map `secrets: {{API_TOKEN: my-secret}}` in tools.yaml, and store the value in your secret backend (`BOT_SECRET__<profile>__my_secret`).\n\
  - Egress: list `egress: [\"api.example.com:443\"]` in tools.yaml. For remote HTTP servers, omit `input` and run `bot mcp sync <profile>/{skill_dir}` to lock definitions.\n\
\n\
4. Validate:\n\
  bot validate\n\
See docs/first-tool.md for a full example and minimal MCP server.",
        policy = policy_lines.join("\n"),
    )
}

pub fn tool(
    dir: &Path,
    id: &str,
    class: ToolClass,
    server: Option<&str>,
    agent: Option<&str>,
) -> Result<String, String> {
    let (skill, tool_name) = parse_tool_id(id)?;
    if let Some(alias) = server
        && !is_name(alias)
    {
        return Err(format!(
            "server alias {alias:?} does not match naming rule ({NAME_RULE}) — e.g. --server my-server"
        ));
    }
    let writer = open(dir)?;
    let location = find_skill(&writer, skill, agent)?;
    let limits = Limits::default();
    let skill_dir = location.dir_path();
    let rel = format!("{skill_dir}/tools.yaml");
    let scope = format!("{skill}.{tool_name}");

    let skill_parts: Vec<&str> = location.parts.iter().map(String::as_str).collect();
    // Serialize concurrent `bot add tool` read/validate/replace on the skill directory without lock files.
    let _skill_lock = writer.lock_dir(&skill_parts)?;

    let mut tools_parts = skill_parts.clone();
    tools_parts.push("tools.yaml");
    let mut lock_parts = skill_parts;
    lock_parts.push("tools.lock.yaml");
    let lock_bytes = if writer.exists(&lock_parts)? {
        Some(writer.read_regular(&lock_parts, limits.max_yaml_bytes)?)
    } else {
        None
    };

    if !writer.exists(&tools_parts)? {
        let server_alias = server.unwrap_or(skill);
        let content = new_tools_yaml(server_alias, tool_name, class, &scope);
        parse_tools_yaml(
            skill,
            &rel,
            content.as_bytes(),
            lock_bytes.as_deref(),
            true,
            &limits,
        )
        .map_err(|issues| {
            format!(
                "generated tools.yaml failed validation:\n{}",
                format_issues(&issues)
            )
        })?;
        finish(writer, &rel, &content)?;
        return Ok(next_steps(
            true,
            &rel,
            &skill_dir,
            &scope,
            class,
            server_alias,
            location.agent_path().as_deref(),
        ));
    }

    let existing_bytes = writer.read_regular(&tools_parts, limits.max_yaml_bytes)?;
    let existing = parse_tools_yaml(
        skill,
        &rel,
        &existing_bytes,
        lock_bytes.as_deref(),
        false,
        &limits,
    )
    .map_err(|issues| {
        format!(
            "existing {rel} has validation errors — fix them before adding a tool:\n{}",
            format_issues(&issues)
        )
    })?;
    if existing.tool_ids.iter().any(|known| known == tool_name) {
        return Err(format!(
            "tool `{scope}` already exists in {rel} — refusing to overwrite; choose another tool name or edit {rel} directly"
        ));
    }
    if let Some(given) = server
        && given != existing.server
    {
        return Err(format!(
            "{rel} is already bound to server `{}` (differs from `--server {given}`). Tools in one skill share a server; omit `--server`, pass `{}` instead, or create a new skill",
            existing.server, existing.server
        ));
    }
    let existing_rules_ok = parse_tools_yaml(
        skill,
        &rel,
        &existing_bytes,
        lock_bytes.as_deref(),
        true,
        &limits,
    )
    .is_ok();
    let solo = new_tools_yaml(&existing.server, tool_name, class, &scope);
    parse_tools_yaml(skill, &rel, solo.as_bytes(), None, true, &limits).map_err(|issues| {
        format!(
            "new tool entry failed validation; file was not modified:\n{}",
            format_issues(&issues)
        )
    })?;
    let expected_new = new_tool_value(tool_name, class, &scope);
    let existing_text =
        std::str::from_utf8(&existing_bytes).map_err(|_| format!("{rel} is not valid UTF-8"))?;
    let verify = |text: &str| {
        parse_tools_yaml(
            skill,
            &rel,
            text.as_bytes(),
            lock_bytes.as_deref(),
            existing_rules_ok,
            &limits,
        )
        .ok()
        .filter(|doc| matches_appended(&existing, doc, &expected_new))
        .is_some()
    };
    let Some(updated) = append_preserving_comments(existing_text, tool_name, class, &scope)
        .filter(|candidate| verify(candidate))
    else {
        let snippet = format_entry(2, tool_name, class, &scope, "\n");
        return Err(format!(
            "cannot append to {rel} while preserving its formatting and comments (for example, a non-empty flow array `tools: [...]`); file was not modified.\n\
             Paste this entry into the `tools` list of {rel} manually:\n\n{snippet}"
        ));
    };
    if updated.len() as u64 > limits.max_yaml_bytes {
        return Err(format!(
            "{rel} would exceed the size limit ({} bytes); file was not modified",
            limits.max_yaml_bytes
        ));
    }
    writer.replace_regular(&rel, &updated)?;
    Ok(next_steps(
        false,
        &rel,
        &skill_dir,
        &scope,
        class,
        &existing.server,
        location.agent_path().as_deref(),
    ))
}
