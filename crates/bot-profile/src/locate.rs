//! `bot mcp sync`가 대상 프로필과 스킬을 정하는 길. `validate`와 같은 로더 규칙을 그대로 쓴다: 루트 목록(루트 자체는 링크여도 되지만 그 아래 링크·낱개 파일·이름 규칙 위반은
//! 거절), 프로필 트리(링크·특수 파일·하드링크·과대 입력 거절, 파일은 한 번만 읽어 메모리에 고정), 폴더 구조, `profile.yaml`의 스키마와 `id`=폴더 이름.
//! 프로필 전체의 의미 검사(참조·권한)는 하지 않는다 — 아직 잠금이 없는 스킬이 프로필 검증을 막아 첫 동기화를 못 하게 하지 않는다. 프로필을 이렇게 확정하지 못하면
//! 호출하는 쪽은 서버에 접속하지도 자격을 읽지도 않는다.
use crate::{
    issue::{Issue, Issues},
    layout::{self, SkillFiles},
    limits::Limits,
    lock, parse, profile,
    scan::{self, Tree},
    schemas::bundled,
    text, tools, yaml,
};
use bot_core::id::ProfileId;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// 확정한 프로필과 스킬. 바이트는 트리를 읽을 때 고정한 것이다(디스크를 다시 읽지 않는다).
#[derive(Debug)]
pub struct Located {
    /// `profile.yaml`의 `id`(폴더 이름과 같다)
    pub profile: ProfileId,
    /// 프로필 폴더(루트 아래 실제 경로)
    pub profile_dir: PathBuf,
    /// 프로필 폴더 기준 스킬 폴더 경로(`agents/main/skills/weather`)
    pub skill: String,
    /// 스킬 폴더 이름
    pub name: String,
    pub skill_md: Vec<u8>,
    pub tools_yaml: Vec<u8>,
    /// 읽은 프로필의 총 바이트와 그 안의 기존 잠금 바이트
    total: u64,
    old_lock: u64,
}

fn refuse(code: &'static str, path: &Path, message: &str) -> Vec<Issue> {
    vec![Issue::error(code, path.display().to_string(), message)]
}

/// `dir`을 루트 기준 상대 경로로(링크를 풀지 않는다). 루트 밖이거나 `..`가 들어 있으면 `None`.
fn below_root(root: &Path, dir: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(dir).ok()?;
    let canonical = std::fs::canonicalize(root).ok()?;
    let given = std::path::absolute(root).ok()?;
    // 준 루트를 먼저 본다: 정규 루트를 먼저 보면 `<정규 루트>/../<링크>/…`가 `..`로 시작하는 상대 경로로 잘린다
    let rel = absolute
        .strip_prefix(&given)
        .or_else(|_| absolute.strip_prefix(&canonical))
        .ok()?;
    rel.components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| rel.to_path_buf())
}

/// `root`(BOT_PROFILES) 아래 스킬 폴더 `dir`의 프로필과 스킬을 확정한다. 문제는 모두 고정 코드가 있는 항목이다.
pub fn locate_skill(root: &Path, dir: &Path, limits: &Limits) -> Result<Located, Vec<Issue>> {
    let mut issues = Issues::new(limits.max_issues);
    let listed = scan::list_profiles(root, limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    let not_a_skill = || {
        refuse(
            "not-a-skill",
            dir,
            "BOT_PROFILES 아래 프로필의 스킬 폴더(`<프로필>/skills/<이름>` 또는 `<프로필>/agents/…/skills/<이름>`)가 아니에요",
        )
    };
    let rel = below_root(root, dir).ok_or_else(not_a_skill)?;
    let mut parts = rel
        .components()
        .map(|part| part.as_os_str().to_str().unwrap_or_default().to_owned());
    let folder = parts.next().unwrap_or_default();
    let inside = parts.collect::<Vec<_>>().join("/");
    let (name, profile_dir) = listed
        .into_iter()
        .find(|(name, _)| *name == folder)
        .ok_or_else(not_a_skill)?;
    // 프로필 트리: 링크·특수 파일·하드링크·크기 한도는 `validate`와 같은 스캔이 거절한다
    let tree = scan::read_tree(&profile_dir, "", limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    let found = layout::discover(&tree, limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    // `profile.yaml`: 번들 스키마를 통과하고 `id`가 폴더 이름과 같아야 한다(자격은 이 프로필의 보관소에서 읽는다)
    let value = parse::yaml_of(
        &tree,
        "profile.yaml",
        &bundled().profile,
        limits,
        &mut issues,
    );
    let parts = value.and_then(|value| profile::parse(&value, Some(&name), limits, &mut issues));
    let Some(parts) = parts.filter(|_| !issues.has_errors()) else {
        return Err(issues.into_vec());
    };
    let skill: &SkillFiles = found
        .skills
        .iter()
        .chain(found.agents.iter().flat_map(|agent| agent.skills.iter()))
        .find(|skill| skill.dir == inside)
        .ok_or_else(not_a_skill)?;
    let bytes = |path: &Option<String>| path.as_deref().and_then(|p| tree.file(p));
    let (Some(skill_md), Some(tools_yaml)) = (bytes(&skill.skill_md), bytes(&skill.tools)) else {
        return Err(refuse(
            "missing-file",
            dir,
            "스킬 폴더에 SKILL.md와 tools.yaml이 있어야 해요",
        ));
    };
    Ok(Located {
        profile: ProfileId(parts.id),
        profile_dir,
        skill: skill.dir.clone(),
        name: skill.name.clone(),
        skill_md: skill_md.to_vec(),
        tools_yaml: tools_yaml.to_vec(),
        total: tree.files.values().map(|bytes| bytes.len() as u64).sum(),
        old_lock: bytes(&skill.lock).map_or(0, |lock| lock.len() as u64),
    })
}

impl Located {
    /// 새 `tools.lock.yaml`을 프로필을 읽을 때와 같은 검사로 읽어 본다: 파일 크기 한도(YAML 256 KiB)·프로필 총 크기, 텍스트 규칙, YAML 한도(값 하나 64 KiB 등),
    /// 잠금의 모양과 해시, `tools.yaml`과의 짝(`input` 생략·서버 이름·`lock-name-mismatch`·입력 스키마 어휘). 통과해야 쓴다.
    pub fn verify_lock(&self, lock: &[u8], limits: &Limits) -> Result<(), Vec<Issue>> {
        let path = format!("{}/{}", self.skill, crate::lock::FILE);
        if lock.len() as u64 > limits.max_yaml_bytes {
            return Err(vec![Issue::error(
                "yaml-too-large",
                path,
                "tools.lock.yaml이 한도를 넘어요 — 선언하는 도구를 줄이세요",
            )]);
        }
        if self.total.saturating_sub(self.old_lock) + lock.len() as u64 > limits.max_profile_bytes {
            return Err(vec![Issue::error(
                "too-large-total",
                path,
                "프로필이 너무 커져요",
            )]);
        }
        check_skill(&self.name, &self.skill_md, &self.tools_yaml, lock, limits)
    }
}

/// 스킬 하나의 파일 셋(SKILL.md·tools.yaml·tools.lock.yaml)을 프로필을 읽을 때의 스킬 해석(`parse::skill`)으로 읽어 본다.
pub fn check_skill(
    name: &str,
    skill_md: &[u8],
    tools: &[u8],
    lock: &[u8],
    limits: &Limits,
) -> Result<(), Vec<Issue>> {
    let dir = format!("skills/{name}");
    let mut tree = Tree::default();
    tree.dirs.insert("skills".to_owned());
    tree.dirs.insert(dir.clone());
    let place = |file: &str| format!("{dir}/{file}");
    for (file, bytes) in [
        ("SKILL.md", skill_md),
        ("tools.yaml", tools),
        (crate::lock::FILE, lock),
    ] {
        tree.files.insert(place(file), bytes.to_vec());
    }
    let files = SkillFiles {
        name: name.to_owned(),
        dir: dir.clone(),
        skill_md: Some(place("SKILL.md")),
        tools: Some(place("tools.yaml")),
        lock: Some(place(crate::lock::FILE)),
    };
    let mut issues = Issues::new(limits.max_issues);
    let parsed = parse::skill(&tree, &files, limits, &mut issues);
    if issues.has_errors() || parsed.is_none() {
        let errors: Vec<Issue> = issues
            .into_vec()
            .into_iter()
            .filter(Issue::is_error)
            .collect();
        return Err(if errors.is_empty() {
            vec![Issue::error(
                "skill-unreadable",
                dir,
                "스킬을 읽을 수 없어요",
            )]
        } else {
            errors
        });
    }
    Ok(())
}

/// `tools.yaml` 한 파일을 프로필 로더와 같은 규칙으로 해석한 결과(`bot add tool` 검증·병합용).
#[derive(Debug)]
pub struct ToolsDoc {
    pub server: String,
    pub tool_ids: Vec<String>,
    pub value: Value,
}

/// `tools.yaml` 바이트를 텍스트·YAML·번들 `tools.schema.json`으로 읽고 검증한다.
/// `check_rules`가 참이면 `tools::parse`(예약 ID·중복·`input`/잠금·egress·destination)까지 검사한다.
pub fn parse_tools_yaml(
    skill: &str,
    path: &str,
    bytes: &[u8],
    lock_bytes: Option<&[u8]>,
    check_rules: bool,
    limits: &Limits,
) -> Result<ToolsDoc, Vec<Issue>> {
    let mut issues = Issues::new(limits.max_issues);
    let raw = match text::check_text(path, bytes) {
        Ok(text) => text,
        Err(issue) => return Err(vec![issue]),
    };
    let value = match yaml::parse(path, raw, limits) {
        Ok(value) => value,
        Err(issue) => return Err(vec![issue]),
    };
    for v in bundled().tools.validate(&value) {
        issues.error_at("schema", path, v.pointer, v.message);
    }
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    if check_rules {
        let parsed_lock = match lock_bytes {
            Some(raw_lock) => {
                let lock_path = format!("{skill}/{}", crate::lock::FILE);
                let lock_text = match text::check_text(&lock_path, raw_lock) {
                    Ok(t) => t,
                    Err(issue) => return Err(vec![issue]),
                };
                let lock_val = match yaml::parse(&lock_path, lock_text, limits) {
                    Ok(v) => v,
                    Err(issue) => return Err(vec![issue]),
                };
                match lock::parse(&lock_val, &lock_path, limits, &mut issues) {
                    Some(parsed) => Some(parsed),
                    None => return Err(issues.into_vec()),
                }
            }
            None => None,
        };
        let _ = tools::parse(
            &value,
            skill,
            path,
            limits,
            parsed_lock.as_ref(),
            &mut issues,
        );
        if issues.has_errors() {
            return Err(issues
                .into_vec()
                .into_iter()
                .filter(Issue::is_error)
                .collect());
        }
    }
    let server = value
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_ids = value
        .get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(ToolsDoc {
        server,
        tool_ids,
        value,
    })
}
