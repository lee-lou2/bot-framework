#![allow(dead_code)]
//! 시험 공용: 임시 폴더, 기본 프로필(파일 지도), 호스트 등록부, 한 줄 편집.
use bot_profile::{HostCatalog, Limits, ProfileSet, Report, SlackWorkspace, load};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

pub mod openai;

pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bot-profile-it-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Temp(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub type Files = Vec<(String, String)>;

pub const DELETE: &str = "<<delete>>";

pub const PROFILE: &str = r#"version: 1
id: personal
timezone: Asia/Seoul
model: assistant
roles:
  owner: [run, calendar.read, calendar.delete]
  reader: [calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "service:morning", roles: [reader]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: inbox, adapter: http, endpoint: reminders, agent: main}
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}
  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}
secrets: [calendar-reader]
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/research: [run, calendar.read]
  approve: []
"#;

pub const TOOLS: &str = r#"version: 1
server: demo-agenda
tools:
  - id: list
    class: read
    scopes: [calendar.read]
    secrets: {CALENDAR_TOKEN: calendar-reader}
    egress: ["calendar.example:443"]
    input: {type: object, properties: {}, additionalProperties: false}
  - id: delete
    class: irreversible
    scopes: [calendar.delete]
    input:
      type: object
      required: [event]
      properties: {event: {type: string}}
      additionalProperties: false
  - id: find
    class: read
    external: true
    scopes: [calendar.read]
    input:
      type: object
      required: [query]
      properties: {query: {type: string, minLength: 1}}
      additionalProperties: false
"#;

pub const TRIGGERS: &str = r#"version: 1
triggers:
  - id: morning
    "on": {cron: "* * * * *"}
    principal: "service:morning"
    target: {tool: agenda.list, args: {}}
"#;

/// 기본 프로필 `personal`의 파일들. 호스트 등록부(`host()`)와 짝이다.
pub fn base() -> Files {
    let files = [
        ("profile.yaml", PROFILE),
        ("rules.md", "- 답은 한국어로 짧게 쓴다.\n"),
        (
            "knowledge/guide.md",
            "# 안내\n일정은 agenda 스킬로 다룬다.\n",
        ),
        (
            "agents/main/AGENT.md",
            "오늘 일정을 조회하고 변경 전에는 정확한 일정을 확인한다.\n",
        ),
        ("agents/main/env.yaml", "AGENDA_VIEW: today\n"),
        ("agents/main/triggers.yaml", TRIGGERS),
        ("agents/main/knowledge/habits.md", "아침에 일정을 본다.\n"),
        (
            "agents/main/memory/routine.md",
            "---\nowner: \"local:owner\"\n---\n아침에 일정을 확인한다.\n",
        ),
        (
            "agents/main/skills/agenda/SKILL.md",
            "일정 목록은 list로 조회한다.\n",
        ),
        ("agents/main/skills/agenda/tools.yaml", TOOLS),
        (
            "agents/main/agents/research/AGENT.md",
            "부모가 맡긴 일정 조회를 수행한다.\n",
        ),
    ];
    files
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect()
}

/// 파일 하나를 고친다. `from`을 `DELETE`로 주면 지우고, 파일이 없으면 `to`로 새로 만든다.
pub fn edit(mut files: Files, path: &str, from: &str, to: &str) -> Files {
    let at = files.iter().position(|(p, _)| p == path);
    match (at, from == DELETE) {
        (Some(i), true) => {
            files.remove(i);
        }
        (Some(i), false) => {
            assert!(files[i].1.contains(from), "{path}에 `{from}`가 없어요");
            files[i].1 = files[i].1.replacen(from, to, 1);
        }
        (None, false) => files.push((path.to_owned(), to.to_owned())),
        (None, true) => panic!("{path}가 없어요"),
    }
    files
}

/// 파일 끝에 덧붙인다(트리거를 더 넣을 때).
pub fn append(mut files: Files, path: &str, text: &str) -> Files {
    let file = files
        .iter_mut()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("{path}가 없어요"));
    file.1.push_str(text);
    files
}

/// 검증 없이 묶음 바이트를 만든다(복원 경로 시험용): `BOTPROFILE1\n | u32 개수 | (u32 경로 길이 | 경로 | u64 길이 | 바이트)…`, 경로 오름차순.
pub fn raw_bundle(files: &Files) -> Vec<u8> {
    let mut sorted: Vec<&(String, String)> = files.iter().collect();
    sorted.sort();
    let mut out = b"BOTPROFILE1\n".to_vec();
    out.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
    for (path, text) in sorted {
        out.extend_from_slice(&(path.len() as u32).to_be_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(text.len() as u64).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

pub fn write_profile(root: &Path, name: &str, files: &Files) {
    for (path, content) in files {
        let full = root.join(name).join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
}

/// 기본 프로필이 쓰는 별칭을 모두 등록한 호스트. 두 번째 프로필 `second`도 허용한다.
pub fn host() -> HostCatalog {
    let set = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect();
    let mut host = HostCatalog::demo();
    host.servers = set(&["demo-agenda", "demo-notes"]);
    host.models
        .insert("personal".into(), set(&["assistant", "fast"]));
    host.models.insert("second".into(), set(&["assistant"]));
    host.cli_accounts = set(&["owner", "other"]);
    host.slack.insert(
        "home".into(),
        SlackWorkspace {
            channels: set(&["planning", "ops"]),
            dm: true,
        },
    );
    host.http_endpoints = set(&["reminders", "alerts"]);
    host
}

/// 파일 지도를 임시 폴더에 `personal`로 쓰고 읽어 본다(읽은 뒤 폴더는 지운다).
pub fn check(files: &Files) -> Result<ProfileSet, Report> {
    let root = Temp::new("check");
    write_profile(root.path(), "personal", files);
    load(root.path(), &host(), &Limits::default())
}

pub fn codes(files: &Files) -> Vec<&'static str> {
    match check(files) {
        Ok(_) => Vec::new(),
        Err(report) => report.codes(),
    }
}

pub fn ok(files: &Files) -> ProfileSet {
    match check(files) {
        Ok(set) => set,
        Err(report) => panic!("검증이 실패했어요:\n{report}"),
    }
}

pub const TOOLS_PATH: &str = "agents/main/skills/agenda/tools.yaml";
pub const AGENT_MD: &str = "agents/main/AGENT.md";

pub fn agent(s: &str) -> bot_core::id::AgentId {
    bot_core::id::AgentId(s.to_owned())
}

/// 기본 프로필 형태의 `personal` 스냅샷(검증 통과 필수)
pub fn snapshot(files: &Files) -> bot_core::profile::Snapshot {
    let set = ok(files);
    let snap = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    (**snap).clone()
}

/// 기본 프로필에 형제·손자 에이전트를 더한 가족: main · main/research · main/research/deep · main/other
pub fn family() -> Files {
    let mut files = base();
    for (path, text) in [
        (
            "agents/main/agents/research/agents/deep/AGENT.md",
            "깊이 파고든다.\n",
        ),
        ("agents/main/agents/other/AGENT.md", "다른 갈래.\n"),
    ] {
        files = edit(files, path, "", text);
    }
    files
}
