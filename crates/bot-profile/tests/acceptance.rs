//! roadmap 1단계(프로필) 수락 시험: 예시 검증, 중첩 에이전트, 모호한 바인딩, 반영 전 거절, 같은 바이너리에서 확장.
mod support;

use bot_core::{
    id::{AgentId, ProfileId, ToolId},
    profile::{Adapter, ProfileCatalog, TriggerSource},
    state::{Risk, Visibility},
};
use bot_profile::{HostCatalog, Limits, Registry, load};
use std::{
    fs,
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use support::{DELETE, Temp, base, codes, edit, host, ok, write_profile};

fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

fn agent(s: &str) -> AgentId {
    AgentId(s.to_owned())
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

const SECOND: &str = r#"version: 1
id: second
model: assistant
roles: {owner: [run]}
members: [{identity: "local:other", roles: [owner]}]
bindings:
  - {id: desk, adapter: cli, account: other, agent: main}
  - {id: ops, adapter: slack, workspace: home, channel: ops, agent: main}
"#;

fn second_profile() -> support::Files {
    vec![
        ("profile.yaml".into(), SECOND.into()),
        (
            "agents/main/AGENT.md".into(),
            "두 번째 프로필의 비서.\n".into(),
        ),
    ]
}

#[test]
fn validates_personal_example() {
    let set = load(&examples_root(), &HostCatalog::demo(), &Limits::default())
        .unwrap_or_else(|r| panic!("예시가 검증을 통과하지 못했어요:\n{r}"));
    let snap = set.profile(&personal()).unwrap();
    assert_eq!(snap.revision.0.len(), 64);
    assert_eq!(snap.policy_revision.0.len(), 64);
    let ids: Vec<&str> = snap.agents.keys().map(|a| a.0.as_str()).collect();
    assert_eq!(ids, ["main", "main/research"]);
    assert_eq!(snap.root, agent("main"));
    let main = &snap.agents[&agent("main")];
    let agenda = &main.skills["agenda"];
    let list = &agenda.tools[&ToolId("agenda.list".into())];
    let delete = &agenda.tools[&ToolId("agenda.delete".into())];
    assert_eq!((list.class, delete.class), (Risk::Read, Risk::Irreversible));
    assert_eq!(
        (
            list.server.as_str(),
            delete.scopes.iter().next().unwrap().as_str()
        ),
        ("demo-agenda", "calendar.delete")
    );
    let cron: Vec<_> = main
        .triggers
        .iter()
        .filter(|t| matches!(t.on, TriggerSource::Cron { .. }))
        .collect();
    assert_eq!(cron.len(), 1);
    assert_eq!(cron[0].id, "morning");
    match &cron[0].on {
        TriggerSource::Cron {
            schedule,
            beat,
            principal,
            approver,
        } => {
            assert_eq!(
                (schedule.as_str(), *beat, principal.0.as_str()),
                ("* * * * *", false, "service:morning")
            );
            assert!(approver.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(snap.bindings.len(), 1);
    assert!(
        matches!(&snap.bindings.values().next().unwrap().source, Adapter::Cli { account } if account == "owner")
    );
    assert!(set.warnings().is_empty(), "{:?}", set.warnings());
    let research = &snap.agents[&agent("main/research")];
    assert_eq!(research.description, "일정 조회를 맡는 하위 비서");
    assert_eq!(
        main.description, "main",
        "front matter가 없으면 경로 마지막 이름"
    );
    assert!(main.builtin.is_empty() && research.builtin.is_empty());
    let (rules, main_text, own) = (
        "- 답은 한국어로",
        "오늘 일정을 조회하고",
        "부모가 맡긴 일정 조회를",
    );
    let at = |needle: &str| {
        research
            .instructions
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}"))
    };
    assert!(
        at(rules) < at(main_text) && at(main_text) < at(own),
        "rules → 부모 → 자기 순서"
    );
    assert_eq!(main.env["AGENDA_VIEW"], "today");
    assert_eq!(main.memory_seeds.len(), 1);
    assert_eq!(main.memory_seeds[0].visibility, Visibility::Private);
    assert_eq!(main.memory_seeds[0].owner.0, "local:owner");
    assert!(research.memory_seeds.is_empty(), "시드는 상속하지 않는다");
    assert_eq!(
        research
            .scopes
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["calendar.read", "run"]
    );
    assert_eq!(main.knowledge.len(), 1);
    assert!(
        set.bundle(&personal())
            .is_some_and(|b| b.starts_with(b"BOTPROFILE1\n"))
    );
    let shown = set.render();
    assert!(
        shown.starts_with("검증 통과: 프로필 1개\npersonal 판본 "),
        "{shown}"
    );
    assert!(
        shown.contains("에이전트 2 도구 2 트리거 1 바인딩 1"),
        "{shown}"
    );
}

#[test]
fn discovers_nested_agents() {
    let mut files = base();
    for (path, text) in [
        (
            "agents/main/agents/research/agents/deep/AGENT.md",
            "깊이 파고든다.\n",
        ),
        (
            "agents/main/agents/research/agents/deep/agents/leaf/AGENT.md",
            "잎.\n",
        ),
        ("agents/main/agents/notes/AGENT.md", "메모한다.\n"),
    ] {
        files = edit(files, path, "", text);
    }
    let set = ok(&files);
    let snap = set.profile(&personal()).unwrap();
    let ids: Vec<&str> = snap.agents.keys().map(|a| a.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "main",
            "main/notes",
            "main/research",
            "main/research/deep",
            "main/research/deep/leaf"
        ]
    );
    let children: Vec<&str> = snap
        .agents
        .iter()
        .filter(|(_, a)| a.parent == Some(agent("main")))
        .map(|(id, _)| id.0.as_str())
        .collect();
    assert_eq!(children, ["main/notes", "main/research"]);
    let leaf = &snap.agents[&agent("main/research/deep/leaf")];
    assert_eq!(leaf.parent, Some(agent("main/research/deep")));
    assert_eq!(
        leaf.instructions.split("\n\n").count(),
        5,
        "rules + 네 단계 AGENT.md"
    );
    assert!(snap.agents[&snap.root].parent.is_none());
}

#[test]
fn rejects_ambiguous_bindings() {
    let twice = edit(
        base(),
        "profile.yaml",
        "  - {id: inbox, adapter: http",
        "  - {id: desk2, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http",
    );
    assert_eq!(codes(&twice), ["duplicate-entrance"]);
    let same_id = edit(
        base(),
        "profile.yaml",
        "id: inbox, adapter: http, endpoint: reminders",
        "id: desk, adapter: http, endpoint: reminders",
    );
    assert_eq!(codes(&same_id), ["duplicate-binding-id"]);

    // 다른 프로필이 같은 Slack 채널을 쓰면 활성 프로필 전체에서 모호하다
    let root = Temp::new("ambiguous");
    write_profile(root.path(), "personal", &base());
    let clash = edit(
        second_profile(),
        "profile.yaml",
        "channel: ops",
        "channel: planning",
    );
    write_profile(root.path(), "second", &clash);
    let report = load(root.path(), &host(), &Limits::default()).unwrap_err();
    assert_eq!(report.codes(), ["duplicate-entrance"]);
    assert!(report.errors().next().unwrap().path.starts_with("second/"));
}

#[test]
fn rejects_bad_link_duplicate_binding_and_missing_server_before_swap() {
    let root = Temp::new("swap");
    write_profile(root.path(), "personal", &base());
    let registry = Registry::new();
    let (host, limits) = (host(), Limits::default());
    let first = registry.reload(root.path(), &host, &limits).unwrap();
    assert_eq!(first.generation(), 1);
    let profile_yaml = root.path().join("personal/profile.yaml");
    let tools_yaml = root
        .path()
        .join("personal/agents/main/skills/agenda/tools.yaml");
    let (profile_text, tools_text) = (
        fs::read_to_string(&profile_yaml).unwrap(),
        fs::read_to_string(&tools_yaml).unwrap(),
    );

    // 잘못된 링크
    let link = root.path().join("personal/knowledge/hosts.md");
    std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
    assert!(
        registry
            .reload(root.path(), &host, &limits)
            .unwrap_err()
            .has("symlink")
    );
    assert!(
        Arc::ptr_eq(&registry.set(), &first),
        "실패한 교체는 이전 집합을 그대로 둔다"
    );
    fs::remove_file(&link).unwrap();

    // 중복 바인딩
    let dup = profile_text.replace(
        "  - {id: inbox",
        "  - {id: desk2, adapter: cli, account: owner, agent: main}\n  - {id: inbox",
    );
    fs::write(&profile_yaml, dup).unwrap();
    assert!(
        registry
            .reload(root.path(), &host, &limits)
            .unwrap_err()
            .has("duplicate-entrance")
    );
    assert!(Arc::ptr_eq(&registry.set(), &first));
    fs::write(&profile_yaml, &profile_text).unwrap();

    // 누락된 서버
    fs::write(
        &tools_yaml,
        tools_text.replace("demo-agenda", "gone-server"),
    )
    .unwrap();
    assert!(
        registry
            .reload(root.path(), &host, &limits)
            .unwrap_err()
            .has("missing-server")
    );
    assert!(Arc::ptr_eq(&registry.set(), &first));
    fs::write(&tools_yaml, &tools_text).unwrap();

    // 고치면 같은 바이트이므로 같은 집합(세대 그대로)
    let again = registry.reload(root.path(), &host, &limits).unwrap();
    assert!(Arc::ptr_eq(&again, &first) && again.generation() == 1);
}

#[test]
fn adds_second_profile_and_grandchild_without_code_change() {
    let root = Temp::new("grow");
    write_profile(root.path(), "personal", &base());
    let registry = Registry::new();
    let (host, limits) = (host(), Limits::default());
    let one = registry.reload(root.path(), &host, &limits).unwrap();
    assert_eq!(one.active().len(), 1);

    write_profile(root.path(), "second", &second_profile());
    let grandchild = root
        .path()
        .join("personal/agents/main/agents/research/agents/deep");
    fs::create_dir_all(&grandchild).unwrap();
    fs::write(grandchild.join("AGENT.md"), "손자 역할.\n").unwrap();
    let two = registry.reload(root.path(), &host, &limits).unwrap();
    assert_eq!((two.generation(), two.active().len()), (2, 2));
    assert_ne!(one.revision(), two.revision());
    let snap = two.profile(&personal()).unwrap();
    assert!(snap.agents.contains_key(&agent("main/research/deep")));
    let second = two.profile(&ProfileId("second".into())).unwrap();
    assert!(
        matches!(&second.bindings.values().next().unwrap().source, Adapter::Cli { account } if account == "other")
    );
    let catalog: &dyn ProfileCatalog = &registry;
    assert_eq!(catalog.active().len(), 2);
    assert!(Arc::ptr_eq(
        &catalog.current(&ProfileId("second".into())).unwrap(),
        second
    ));
}

#[test]
fn rejects_unsafe_tree_entries_before_anything_is_read() {
    let limits = Limits::default();
    for case in ["socket", "hardlink", "fifo-like name", "bad name"] {
        let root = Temp::new("unsafe");
        write_profile(root.path(), "personal", &base());
        let dir = root.path().join("personal/knowledge");
        let _sock;
        let expected = match case {
            "socket" => {
                // 소켓 경로는 104바이트 제한이 있어 짧은 곳에서 만든 뒤 옮긴다
                let short = PathBuf::from("/tmp").join(format!("bp{}.s", std::process::id()));
                let _ = fs::remove_file(&short);
                _sock = UnixListener::bind(&short).unwrap();
                fs::rename(&short, dir.join("s.sock")).unwrap();
                "special-file"
            }
            "hardlink" => {
                fs::hard_link(dir.join("guide.md"), dir.join("copy.md")).unwrap();
                "hardlink"
            }
            "bad name" => {
                fs::write(dir.join("a\u{202e}b.md"), "x").unwrap();
                "bad-name"
            }
            _ => {
                fs::write(dir.join("secret.pem"), "x").unwrap();
                "bad-document"
            }
        };
        let report = load(root.path(), &host(), &limits).unwrap_err();
        assert!(report.has(expected), "{case}: {report}");
    }
    let root = Temp::new("sizes");
    write_profile(root.path(), "personal", &base());
    let amplified = Limits {
        max_assembled_bytes: 100,
        ..Limits::default()
    };
    let report = load(root.path(), &host(), &amplified).unwrap_err();
    assert!(report.has("assembled-too-large"), "{report}");
    let tight = Limits {
        max_file_bytes: 20,
        ..Limits::default()
    };
    assert!(
        load(root.path(), &host(), &tight)
            .unwrap_err()
            .has("too-large")
    );
    let few = Limits {
        max_files: 3,
        ..Limits::default()
    };
    assert!(
        load(root.path(), &host(), &few)
            .unwrap_err()
            .has("too-many-files")
    );
}

#[test]
fn removing_the_bad_file_makes_the_same_tree_valid() {
    let root = Temp::new("clean");
    write_profile(root.path(), "personal", &base());
    let stray = root.path().join("personal/notes.txt");
    fs::write(&stray, "x").unwrap();
    assert!(
        load(root.path(), &host(), &Limits::default())
            .unwrap_err()
            .has("unknown-file")
    );
    fs::remove_file(stray).unwrap();
    assert!(load(root.path(), &host(), &Limits::default()).is_ok());
    let none = edit(base(), "agents/main/AGENT.md", DELETE, "");
    assert_eq!(codes(&none), ["missing-file"]);
}

#[test]
fn readers_never_see_a_half_applied_set() {
    let root = Temp::new("readers");
    write_profile(root.path(), "personal", &base());
    let registry = Arc::new(Registry::new());
    let (host, limits) = (host(), Limits::default());
    registry.reload(root.path(), &host, &limits).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (registry, stop) = (registry.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut last = 0;
                while !stop.load(Ordering::Relaxed) {
                    let set = registry.set();
                    assert!(set.generation() >= last, "세대는 거꾸로 가지 않는다");
                    last = set.generation();
                    let has_second = set.profile(&ProfileId("second".into())).is_some();
                    assert_eq!(set.active().len(), if has_second { 2 } else { 1 });
                    assert!(
                        set.bundle(&personal()).is_some()
                            && set.bundle(&ProfileId("second".into())).is_some() == has_second
                    );
                }
            })
        })
        .collect();
    for round in 0..20 {
        if round % 2 == 0 {
            write_profile(root.path(), "second", &second_profile());
        } else {
            fs::remove_dir_all(root.path().join("second")).unwrap();
        }
        registry.reload(root.path(), &host, &limits).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().unwrap();
    }
    assert_eq!(registry.set().generation(), 21, "처음 1 + 바뀐 20번");
}

#[test]
fn replaced_snapshots_stay_reachable_only_while_someone_holds_them() {
    let root = Temp::new("retire");
    write_profile(root.path(), "personal", &base());
    let registry = Registry::new();
    let (host, limits) = (host(), Limits::default());
    registry.reload(root.path(), &host, &limits).unwrap();
    let old = registry.set().profile(&personal()).unwrap().clone();
    fs::write(root.path().join("personal/rules.md"), "- 바뀐 규칙\n").unwrap();
    registry.reload(root.path(), &host, &limits).unwrap();
    let now = registry.set().profile(&personal()).unwrap().clone();
    assert_ne!(old.revision, now.revision);
    let found = registry
        .lookup(&personal(), &old.revision)
        .expect("쥐고 있는 옛 판본은 찾을 수 있다");
    assert!(Arc::ptr_eq(&found, &old));
    assert!(Arc::ptr_eq(
        &registry.lookup(&personal(), &now.revision).unwrap(),
        &now
    ));
    let revision = old.revision.clone();
    drop((old, found));
    assert!(
        registry.lookup(&personal(), &revision).is_none(),
        "아무도 쥐지 않으면 사라진다(묶음으로 다시 만든다)"
    );
}

#[test]
fn a_zero_report_limit_never_turns_a_failure_into_success() {
    let root = Temp::new("quiet");
    write_profile(root.path(), "personal", &base());
    let registry = Registry::new();
    let (host, normal) = (host(), Limits::default());
    let first = registry.reload(root.path(), &host, &normal).unwrap();
    for max_issues in [0, 1] {
        let quiet = Limits {
            max_issues,
            ..Limits::default()
        };
        // 한 프로필에서 오류가 여러 개 나게 한다: 링크 + 알 수 없는 파일 + 스키마 오류
        let dir = root.path().join("personal");
        std::os::unix::fs::symlink("/etc/hosts", dir.join("knowledge/a.md")).unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        let report = load(root.path(), &host, &quiet).unwrap_err();
        assert!(
            !report.is_ok() && report.error_total >= 1,
            "한도 {max_issues}: {report}"
        );
        assert!(report.issues.len() <= max_issues.max(1) * 2);
        assert!(registry.reload(root.path(), &host, &quiet).is_err());
        assert!(
            Arc::ptr_eq(&registry.set(), &first),
            "실패한 교체는 이전 집합을 그대로 둔다"
        );
        fs::remove_file(dir.join("knowledge/a.md")).unwrap();
        fs::remove_file(dir.join("notes.txt")).unwrap();
        // 프로필 폴더 밖의 오류도 마찬가지다
        fs::write(root.path().join("stray.txt"), "x").unwrap();
        let report = load(root.path(), &host, &quiet).unwrap_err();
        assert!(!report.is_ok(), "한도 {max_issues}: {report}");
        fs::remove_file(root.path().join("stray.txt")).unwrap();
    }
    assert!(
        registry
            .reload(
                root.path(),
                &host,
                &Limits {
                    max_issues: 0,
                    ..Limits::default()
                }
            )
            .is_ok()
    );
}
