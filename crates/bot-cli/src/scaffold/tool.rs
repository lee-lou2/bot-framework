//! `bot add tool <스킬>.<도구>`: 스킬 폴더의 `tools.yaml`에 도구 항목을 새로 만들거나 덧붙인다.
//! `profile.yaml`과 `host.json`은 읽지도 고치지도 않고, 권한·호스트 설정 조각을 출력만 한다.
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

/// YAML 1.2/1.1 모두에서 문자열로 읽히도록 예약 단어는 큰따옴표로 감싼다.
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
            "도구 ID {id:?}은(는) `<스킬>.<도구>` 형식이어야 해요(둘 다 {NAME_RULE}) — 예: agenda.search"
        ));
    };
    if !is_name(skill) {
        return Err(format!(
            "스킬 이름 {skill:?}이(가) 이름 규칙({NAME_RULE})에 맞지 않아요 — 예: agenda.search"
        ));
    }
    if !is_name(tool) {
        return Err(format!(
            "도구 이름 {tool:?}이(가) 이름 규칙({NAME_RULE})에 맞지 않아요 — 예: agenda.search"
        ));
    }
    if builtin::reserved(id) {
        return Err(format!(
            "`{id}`는 호스트 내장 도구 이름이라 선언할 수 없어요 — 다른 도구 이름을 써 주세요"
        ));
    }
    Ok((skill, tool))
}

struct SkillLocation {
    /// `None`이면 프로필 루트 `skills/<스킬>`, `Some(계보)`면 해당 에이전트 아래 스킬.
    agent: Option<Vec<String>>,
    /// 프로필 폴더 기준 경로 조각(예: `["agents", "main", "skills", "agenda"]`).
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
                "에이전트 {agent_path}이(가) 없어요 — 먼저 `bot add agent {agent_path}`로 만들어 주세요"
            ));
        }
        let mut parts = agent_dir(&scope);
        parts.extend(["skills", skill]);
        let mut skill_md = parts.clone();
        skill_md.push("SKILL.md");
        if !writer.file_exists(&skill_md)? {
            return Err(format!(
                "스킬 `{skill}`이(가) 에이전트 {agent_path} 아래에 없어요 — 먼저 `bot add skill {skill} --agent {agent_path}`로 스킬을 만들어 주세요"
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
            "스킬 `{skill}`을(를) 찾을 수 없어요 — 먼저 `bot add skill {skill}`(또는 `--agent <에이전트 경로>`)로 스킬을 만들어 주세요"
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
                "스킬 `{skill}`이(가) 여러 곳에 있어요({}) — `--agent <에이전트 경로>`로 어느 스킬에 더할지 지정해 주세요",
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

/// 기존 `tools.yaml`의 주석·줄 끝(`\r\n`/`\n`)·키 순서·들여쓰기를 유지하며 새 항목을 삽입한다.
/// 비어 있지 않은 플로우 배열 등 주석 보존 편집이 불가능하면 `None`을 돌려주고 원본을 건드리지 않는다.
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

    // `tools: []`(+ 선택 인라인 주석) 형태면 그 자리에서 블록 목록으로 바꾼다
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

    // `tools:` 뒤에 인라인 값이 남아 있으면(예: `tools: [{...}]`) 주석을 잃지 않도록 수동 편집으로 넘긴다
    let after_trim = after_key.trim_start_matches([' ', '\t']);
    if !after_trim.is_empty() && !after_trim.starts_with('#') {
        return None;
    }

    // `tools:` 블록이 끝나는 지점(다음 최상위 키·문서 끝 표식 직전)과 첫 항목의 들여쓰기를 찾는다
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
        // 들여쓰기 0칸 블록 목록(`- id: ...`)은 `tools:` 항목이다
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
            "만들었어요: {rel} (도구 {full_id}, 등급 {})",
            class_label(class)
        )
    } else {
        format!(
            "더했어요: {rel}에 도구 {full_id}(등급 {})를 추가했어요",
            class_label(class)
        )
    };
    let mut policy_lines = vec![format!("    scopes: [..., {full_id}]")];
    if let Some(agent) = agent_path.filter(|a| a.contains('/')) {
        policy_lines.push("    agents:".to_owned());
        policy_lines.push(format!(
            "      {agent}: [..., {full_id}]   # policy.agents를 쓰는 경우"
        ));
    }
    let approval_note = match class {
        ToolClass::Read => String::new(),
        ToolClass::Write => {
            policy_lines.push(format!(
                "    approve: [..., {full_id}]   # 쓰기 도구에 사람 승인을 요구하려면 추가"
            ));
            String::new()
        }
        ToolClass::Irreversible => {
            "\n  # class: irreversible 도구는 policy.approve에 적지 않아도 항상 사람 승인을 거쳐요."
                .to_owned()
        }
    };
    format!(
        "{verb}\n\
권한과 호스트 설정은 자동으로 바꾸지 않았어요. 아래 네 곳을 맞춰 주세요:\n\
\n\
1. 호스트 설정(host.json)의 `servers`에 도구 서버를 등록해요(로컬 stdio 예시; 원격 HTTP는 docs/plugins.md 참고):\n\
  \"servers\": {{\n\
    \"{server}\": {{\n\
      \"root\": \"/절대/경로/servers/{server}\",\n\
      \"command\": [\"bin/server\", \"--stdio\"]\n\
    }}\n\
  }}\n\
\n\
2. profile.yaml에서 필요한 역할과 정책 상한에만 스코프를 허용해요:\n\
  roles:\n\
    owner: [..., {full_id}]\n\
  policy:\n\
{policy}{approval_note}\n\
\n\
3. (선택) 일반 설정·비밀·외부 통신·원격 잠금이 필요하면:\n\
  - 일반 설정: 에이전트 `env.yaml`에 키를 넣고 tools.yaml에 `env: [KEY]`를 적어요.\n\
  - 비밀: profile.yaml `secrets: [my-secret]`과 tools.yaml `secrets: {{API_TOKEN: my-secret}}`을 맞추고 보관소(`BOT_SECRET__<프로필>__my_secret`)에 값을 넣어요.\n\
  - 외부 통신: tools.yaml `egress: [\"api.example.com:443\"]`를 적어요. 원격 HTTP 서버의 정의를 잠그려면 `input`을 빼고 `bot mcp sync <프로필>/{skill_dir}`을 실행해요.\n\
\n\
4. 검증해요:\n\
  bot validate\n\
자세한 예시와 최소 MCP 서버 코드는 docs/first-tool.md에 있어요.",
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
            "서버 별칭 {alias:?}이(가) 이름 규칙({NAME_RULE})에 맞지 않아요 — 예: --server my-server"
        ));
    }
    let writer = open(dir)?;
    let location = find_skill(&writer, skill, agent)?;
    let limits = Limits::default();
    let skill_dir = location.dir_path();
    let rel = format!("{skill_dir}/tools.yaml");
    let scope = format!("{skill}.{tool_name}");

    let skill_parts: Vec<&str> = location.parts.iter().map(String::as_str).collect();
    // 같은 스킬 폴더의 동시 `bot add tool` 읽기·검증·교체를 잠금 파일 없이 직렬화한다
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
                "만든 tools.yaml이 검증을 통과하지 못했어요:\n{}",
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
            "기존 {rel}에 오류가 있어 항목을 더할 수 없어요 — 먼저 아래 문제를 고쳐 주세요:\n{}",
            format_issues(&issues)
        )
    })?;
    if existing.tool_ids.iter().any(|known| known == tool_name) {
        return Err(format!(
            "도구 `{scope}`은(는) 이미 {rel}에 있어요 — 덮어쓰지 않아요. 다른 도구 이름을 쓰거나 {rel}을 직접 고쳐 주세요"
        ));
    }
    if let Some(given) = server
        && given != existing.server
    {
        return Err(format!(
            "{rel}은(는) 이미 서버 `{}`에 연결돼 있어요(`--server {given}`과 달라요). 같은 스킬의 도구는 한 서버를 쓰므로 `--server`를 생략하거나 `{}`로 맞춰 주세요(다른 서버라면 새 스킬을 만들어 주세요)",
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
    // 새 도구 단독 항목도 스키마·잠금·규칙을 항상 통과해야 한다
    let solo = new_tools_yaml(&existing.server, tool_name, class, &scope);
    parse_tools_yaml(skill, &rel, solo.as_bytes(), None, true, &limits).map_err(|issues| {
        format!(
            "새 도구 항목이 검증을 통과하지 못해 아무것도 바꾸지 않았어요:\n{}",
            format_issues(&issues)
        )
    })?;
    let expected_new = new_tool_value(tool_name, class, &scope);
    let existing_text = std::str::from_utf8(&existing_bytes)
        .map_err(|_| format!("{rel}이(가) UTF-8이 아니에요"))?;
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
            "기존 {rel}의 서식·주석을 보존한 채 자동으로 덧붙일 수 없어 파일을 바꾸지 않았어요(예: 비어 있지 않은 플로우 배열 `tools: [...]`).\n\
             아래 항목을 {rel}의 `tools` 목록에 직접 붙여 넣어 주세요:\n\n{snippet}"
        ));
    };
    if updated.len() as u64 > limits.max_yaml_bytes {
        return Err(format!(
            "{rel}이(가) 크기 한도({} 바이트)를 넘어 아무것도 바꾸지 않았어요",
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
