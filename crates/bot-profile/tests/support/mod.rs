#![allow(dead_code)]
//! Shared test fixtures, temporary directories, host catalog, and single-file edit helpers.
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

/// Base `personal` profile fixture paired with `host()`.
pub fn base() -> Files {
    let files = [
        ("profile.yaml", PROFILE),
        ("rules.md", "- Keep answers brief and in English.\n"),
        (
            "knowledge/guide.md",
            "# Guide\nUse the agenda skill to manage events.\n",
        ),
        (
            "agents/main/AGENT.md",
            "Check today's schedule and confirm the exact event before making changes.\n",
        ),
        ("agents/main/env.yaml", "AGENDA_VIEW: today\n"),
        ("agents/main/triggers.yaml", TRIGGERS),
        (
            "agents/main/knowledge/habits.md",
            "Check the schedule in the morning.\n",
        ),
        (
            "agents/main/memory/routine.md",
            "---\nowner: \"local:owner\"\n---\nCheck the schedule every morning.\n",
        ),
        (
            "agents/main/skills/agenda/SKILL.md",
            "Use list to view the schedule.\n",
        ),
        ("agents/main/skills/agenda/tools.yaml", TOOLS),
        (
            "agents/main/agents/research/AGENT.md",
            "Handle schedule lookups delegated by the parent agent.\n",
        ),
    ];
    files
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect()
}

/// Edits one file in place, deletes it when `from == DELETE`, or inserts it if absent.
pub fn edit(mut files: Files, path: &str, from: &str, to: &str) -> Files {
    let at = files.iter().position(|(p, _)| p == path);
    match (at, from == DELETE) {
        (Some(i), true) => {
            files.remove(i);
        }
        (Some(i), false) => {
            assert!(
                files[i].1.contains(from),
                "{path} does not contain `{from}`"
            );
            files[i].1 = files[i].1.replacen(from, to, 1);
        }
        (None, false) => files.push((path.to_owned(), to.to_owned())),
        (None, true) => panic!("missing file {path}"),
    }
    files
}

pub fn append(mut files: Files, path: &str, text: &str) -> Files {
    let file = files
        .iter_mut()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("missing file {path}"));
    file.1.push_str(text);
    files
}

/// Encodes a `BOTPROFILE1` bundle without validation for restore-path tests.
/// Layout: `BOTPROFILE1\n | u32 count | (u32 path_len | path | u64 len | bytes)...` sorted by path.
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

/// Host catalog registering all aliases used by `base()` and the `second` profile fixture.
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
        Err(report) => panic!("validation failed:\n{report}"),
    }
}

pub const TOOLS_PATH: &str = "agents/main/skills/agenda/tools.yaml";
pub const AGENT_MD: &str = "agents/main/AGENT.md";

pub fn agent(s: &str) -> bot_core::id::AgentId {
    bot_core::id::AgentId(s.to_owned())
}

pub fn snapshot(files: &Files) -> bot_core::profile::Snapshot {
    let set = ok(files);
    let snap = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    (**snap).clone()
}

/// Extends `base()` with sibling and grandchild agents: `main`, `main/research`, `main/research/deep`, `main/other`.
pub fn family() -> Files {
    let mut files = base();
    for (path, text) in [
        (
            "agents/main/agents/research/agents/deep/AGENT.md",
            "Dig deeper.\n",
        ),
        ("agents/main/agents/other/AGENT.md", "Other branch.\n"),
    ] {
        files = edit(files, path, "", text);
    }
    files
}
