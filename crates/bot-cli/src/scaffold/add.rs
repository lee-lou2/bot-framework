//! `bot add agent|skill`: 프로필 폴더에 AGENT.md·SKILL.md 뼈대를 새로 만든다. 이름 조각은 모두 `is_name`으로 검사하고,
//! 도구(tools.yaml)와 profile.yaml은 만들지도 고치지도 않는다(권한 상한은 안내만 한다).
use super::{files::Writer, templates};
use bot_core::id::is_name;
use bot_profile::Limits;
use std::path::Path;

const NAME_RULE: &str = "소문자로 시작하는 소문자·숫자·하이픈, 63자 이하";

/// 에이전트 경로(`main/cs`)를 이름 조각으로 나눈다. 모든 조각이 이름 규칙을 지켜야 한다.
fn lineage(path: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.iter().find(|part| !is_name(part)) {
        Some(bad) => Err(format!(
            "에이전트 경로 {path:?}의 조각 {bad:?}이(가) 이름 규칙({NAME_RULE})에 맞지 않아요 — 예: main/cs"
        )),
        None => Ok(parts),
    }
}

/// 계보 → 프로필 폴더 안의 경로 조각: `main/cs` → `agents/main/agents/cs`
fn agent_dir<'a>(lineage: &[&'a str]) -> Vec<&'a str> {
    lineage.iter().flat_map(|part| ["agents", *part]).collect()
}

fn open(dir: &Path) -> Result<Writer, String> {
    let writer = Writer::new(dir)?;
    if !writer.file_exists(&["profile.yaml"])? {
        return Err(format!(
            "프로필 폴더가 아니에요(profile.yaml이 없어요): {} — 프로필 폴더 안에서 실행하거나 --dir <프로필 폴더>를 지정해 주세요",
            dir.display()
        ));
    }
    Ok(writer)
}

/// 에이전트 폴더(AGENT.md가 있는 폴더)가 이미 있는가
fn has_agent(writer: &Writer, lineage: &[&str]) -> Result<bool, String> {
    let mut parts = agent_dir(lineage);
    parts.push("AGENT.md");
    writer.file_exists(&parts)
}

/// 파일 하나를 만든다. 실패하면 우리가 만든 빈 폴더만 지우고(파일은 이름으로 안전하게 지울 수 없어 남긴다) 남은 파일의 경로를 알린다.
fn finish(mut writer: Writer, rel: &str, text: &str) -> Result<(), String> {
    writer.write(rel, text).map_err(|error| {
        let left = writer.undo();
        if left.is_empty() {
            error
        } else {
            format!(
                "{error}\n만들다 만 파일이 남았어요(확인해서 지워 주세요): {}",
                left.join(", ")
            )
        }
    })
}

pub fn agent(dir: &Path, path: &str) -> Result<String, String> {
    let lineage = lineage(path)?;
    let max = Limits::default().max_agent_depth;
    if lineage.len() > max {
        return Err(format!(
            "에이전트 계보는 {max}단계까지예요(지금 {}단계)",
            lineage.len()
        ));
    }
    let writer = open(dir)?;
    let (leaf, parents) = lineage.split_last().ok_or("에이전트 경로가 비었어요")?;
    if parents.is_empty() {
        // 루트 에이전트는 하나뿐이다: 다른 루트가 있으면 만들지 않는다(같은 이름이면 아래 require_absent가 거절한다)
        let roots = writer.child_dirs(&["agents"])?;
        if !roots.is_empty() && !roots.iter().any(|root| root == leaf) {
            return Err(format!(
                "루트 에이전트는 하나뿐이에요(이미 있는 것: {}) — 하위 에이전트라면 {}/{leaf}처럼 경로로 적어 주세요",
                roots.join(", "),
                roots[0]
            ));
        }
    } else if !has_agent(&writer, parents)? {
        let parent = parents.join("/");
        return Err(format!(
            "상위 에이전트 {parent}이(가) 없어요 — 먼저 `bot add agent {parent}`로 만들어 주세요"
        ));
    }
    let target = agent_dir(&lineage);
    writer.require_absent(&target)?;
    let rel = format!("{}/AGENT.md", target.join("/"));
    finish(writer, &rel, &templates::agent_md(leaf))?;
    let permission = if parents.is_empty() {
        "루트 에이전트는 profile.yaml의 policy.scopes 상한을 따라요".to_owned()
    } else {
        format!(
            "이 에이전트는 부모({})의 권한 상한을 이어받아요. 줄이려면 profile.yaml의 policy.agents에 `{path}: [run]`처럼 직접 적어 주세요",
            parents.join("/")
        )
    };
    Ok(format!(
        "만들었어요: {rel}\n다음:\n  - AGENT.md의 description과 지침을 채워 주세요.\n  - 권한은 바꾸지 않았어요. {permission}.\n  - 부모 에이전트가 이 에이전트에게 맡길 일을 알도록 부모의 AGENT.md나 스킬에 적어 주세요.\n  - `bot validate`로 확인해 주세요."
    ))
}

/// 같은 이름의 스킬이 보이는 범위에 이미 있으면 그 위치. 스킬은 만든 범위와 그 아래 모든 에이전트에게 보이므로, 같은 이름이
/// 프로필 전체·조상·자손 어디에 있든 검증이 이름 충돌로 거절한다. 열어 둔 프로필 폴더(fd)로만 읽는다.
fn skill_conflict(writer: &Writer, scope: &[&str], name: &str) -> Option<String> {
    let exists = |parts: &[&str]| writer.exists(parts).unwrap_or(false);
    if !scope.is_empty() && exists(&["skills", name]) {
        return Some(format!("skills/{name}"));
    }
    let limits = Limits::default();
    let mut budget = limits.max_dirs;
    let mut stack: Vec<Vec<String>> = writer
        .child_dirs(&["agents"])
        .ok()?
        .into_iter()
        .map(|root| vec![root])
        .collect();
    while let Some(lineage) = stack.pop() {
        budget = budget.checked_sub(1)?;
        let names: Vec<&str> = lineage.iter().map(String::as_str).collect();
        // 한쪽 계보가 다른 쪽의 앞부분이면(조상·같은 곳·자손) 서로 보이는 사이다
        let common = names.len().min(scope.len());
        let here = agent_dir(&names);
        if names[..common] == scope[..common] {
            let mut skill = here.clone();
            skill.extend(["skills", name]);
            if exists(&skill) {
                return Some(skill.join("/"));
            }
        }
        if lineage.len() >= limits.max_agent_depth {
            continue;
        }
        let mut children = here;
        children.push("agents");
        for child in writer.child_dirs(&children).unwrap_or_default() {
            let mut next = lineage.clone();
            next.push(child);
            stack.push(next);
        }
    }
    None
}

pub fn skill(dir: &Path, name: &str, agent: Option<&str>) -> Result<String, String> {
    if !is_name(name) {
        return Err(format!(
            "스킬 이름 {name:?}은(는) 이름 규칙({NAME_RULE})에 맞지 않아요 — 예: triage"
        ));
    }
    let scope = agent.map(lineage).transpose()?.unwrap_or_default();
    let writer = open(dir)?;
    if !scope.is_empty() && !has_agent(&writer, &scope)? {
        return Err(format!(
            "에이전트 {}이(가) 없어요 — 먼저 `bot add agent {}`로 만들어 주세요",
            scope.join("/"),
            scope.join("/")
        ));
    }
    let mut target = agent_dir(&scope);
    target.extend(["skills", name]);
    writer.require_absent(&target)?;
    if let Some(found) = skill_conflict(&writer, &scope, name) {
        return Err(format!(
            "같은 이름의 스킬이 이미 보여요: {found} — 이름이 겹치면 검증이 거절해요. 다른 이름을 쓰거나 기존 스킬을 고쳐 주세요"
        ));
    }
    let rel = format!("{}/SKILL.md", target.join("/"));
    finish(writer, &rel, &templates::skill_md(name))?;
    Ok(format!(
        "만들었어요: {rel}\n다음:\n  - SKILL.md의 description과 순서를 채워 주세요(맨 위 `---` 머리말도 지침 글의 일부로 모델에 전달돼요).\n  - 도구(tools.yaml)와 권한은 만들지 않았어요. 도구가 필요하면 tools.yaml을 직접 만들고, 필요한 스코프는 profile.yaml의 roles·policy에 직접 허용해 주세요 — 권한 상한은 자동으로 바꾸지 않아요.\n  - `bot validate`로 확인해 주세요."
    ))
}
