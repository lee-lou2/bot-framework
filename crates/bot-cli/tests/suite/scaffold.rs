//! `bot init`·`bot add`를 실제 `bot` 프로세스로 확인한다: 세 템플릿이 만든 폴더가 바로 `bot validate`를 통과하고, 있는 것은 덮어쓰지 않으며,
//! 에이전트·스킬 뼈대가 올바른 자리·내용으로 들어가 검증을 통과하고, 권한 상한(profile.yaml)은 건드리지 않는다. DB·srt가 필요 없다.
use crate::process_support::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn init(work: &Path, name: &str, template: &str) -> std::process::Output {
    bot(
        &[],
        &["init", name, "--template", template, "--dir", s(work)],
    )
}

fn profile_dir(work: &Path, name: &str) -> PathBuf {
    work.join(name).join("profiles").join(name)
}

fn validate(work: &Path, name: &str) -> std::process::Output {
    let profiles = work.join(name).join("profiles");
    let host = work.join(name).join("host.json");
    bot(
        &[
            ("BOT_PROFILES", s(&profiles)),
            ("BOT_HOST_CONFIG", s(&host)),
        ],
        &["validate"],
    )
}

fn add(profile: &Path, args: &[&str]) -> std::process::Output {
    let mut full = vec!["add"];
    full.extend_from_slice(args);
    full.extend(["--dir", s(profile)]);
    bot(&[], &full)
}

/// 폴더 안의 모든 경로와 파일 내용(링크는 따라가지 않는다). 폴더는 빈 내용으로 적는다.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let key = path.strip_prefix(root).unwrap().display().to_string();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.insert(key, Vec::new());
                walk(&path, root, out);
            } else if kind.is_file() {
                out.insert(key, fs::read(&path).unwrap());
            } else {
                out.insert(key, b"<link>".to_vec());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn every_template_validates_right_after_init() {
    for (template, agents) in [("personal", 1), ("team", 4), ("company", 6)] {
        let work = tempfile::tempdir().unwrap();
        // 새 폴더를 만드는 명령은 환경 설정을 읽지 않는다: 잘못된 BOT_ 변수가 있어도 시작할 수 있다
        let made = bot(
            &[("BOT_SECRETS", "vault")],
            &[
                "init",
                "demo-bot",
                "--template",
                template,
                "--dir",
                s(work.path()),
            ],
        );
        assert!(made.status.success(), "{template}: {}", text(&made));
        let shown = text(&made);
        assert!(
            shown.contains(&format!("템플릿 {template}, 에이전트 {agents}개")),
            "{shown}"
        );
        assert!(shown.contains("export BOT_PROFILES="), "{shown}");
        assert_eq!(
            fs::read_dir(work.path()).unwrap().count(),
            1,
            "임시 폴더가 남지 않는다"
        );
        let checked = validate(work.path(), "demo-bot");
        assert!(checked.status.success(), "{template}: {}", text(&checked));
        let report = text(&checked);
        assert!(report.contains("검증 통과: 프로필 1개"), "{report}");
        assert!(
            report.contains(&format!("에이전트 {agents} 도구 0 트리거 0 바인딩 1")),
            "{template}: {report}"
        );
        // 호스트 설정 예시는 데모 모델과 이 OS 사용자의 CLI 계정만 담는다
        let host: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(work.path().join("demo-bot/host.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(host["gateway"]["cli"]["accounts"]["owner"]["uid"], my_uid());
        assert_eq!(host["models"]["demo-bot"][0], "assistant");
        assert_eq!(host["providers"]["assistant"]["kind"], "demo");
        let profile =
            fs::read_to_string(profile_dir(work.path(), "demo-bot").join("profile.yaml")).unwrap();
        assert!(profile.contains("id: \"demo-bot\""), "{profile}");
    }
}

#[test]
fn init_output_is_deterministic() {
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for work in [&one, &two] {
        assert!(init(work.path(), "same", "team").status.success());
    }
    assert_eq!(snapshot(one.path()), snapshot(two.path()));
}

#[test]
fn init_never_overwrites_and_rejects_bad_names() {
    let work = tempfile::tempdir().unwrap();
    fs::create_dir(work.path().join("mine")).unwrap();
    fs::write(work.path().join("mine/keep.txt"), "mine").unwrap();
    fs::write(work.path().join("a-file"), "file").unwrap();
    let before = snapshot(work.path());
    for name in ["mine", "a-file"] {
        let out = init(work.path(), name, "personal");
        assert_eq!(out.status.code(), Some(1), "{name}: {}", text(&out));
        assert!(text(&out).contains("이미 있어요"), "{}", text(&out));
    }
    assert_eq!(snapshot(work.path()), before, "있는 폴더·파일은 그대로다");
    // 같은 이름으로 두 번째 init은 첫 결과를 건드리지 않는다
    assert!(init(work.path(), "first", "team").status.success());
    let first = snapshot(work.path());
    assert!(!init(work.path(), "first", "personal").status.success());
    assert_eq!(snapshot(work.path()), first);
    // 이름 규칙에 맞지 않거나 경로를 벗어나려는 이름은 아무것도 만들지 않는다
    let inner = work.path().join("inner");
    fs::create_dir(&inner).unwrap();
    let long = "a".repeat(64);
    for bad in [
        "../evil",
        "Bad",
        "a/b",
        "x-",
        "a--b",
        "",
        "..",
        "Ünï",
        long.as_str(),
    ] {
        let out = init(&inner, bad, "personal");
        assert_eq!(out.status.code(), Some(1), "{bad:?}: {}", text(&out));
        assert!(text(&out).contains("소문자"), "{bad:?}: {}", text(&out));
    }
    assert!(snapshot(&inner).is_empty());
    assert!(!work.path().join("evil").exists());
    // 없는 --dir은 만들지 않고 알린다
    let missing = work.path().join("not-here");
    let out = init(&missing, "x", "personal");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("폴더가 없어요"), "{}", text(&out));
    assert!(!missing.exists());
}

#[test]
fn add_places_skeletons_where_validate_accepts_them_and_leaves_permissions_alone() {
    let work = tempfile::tempdir().unwrap();
    assert!(init(work.path(), "demo", "team").status.success());
    let profile = profile_dir(work.path(), "demo");
    let yaml_before = fs::read(profile.join("profile.yaml")).unwrap();

    let agent = add(&profile, &["agent", "main/support"]);
    assert!(agent.status.success(), "{}", text(&agent));
    assert!(
        text(&agent).contains("policy.agents"),
        "권한은 안내만 한다: {}",
        text(&agent)
    );
    assert!(
        add(&profile, &["agent", "main/support/triage"])
            .status
            .success()
    );
    let nested = profile.join("agents/main/agents/support/agents/triage/AGENT.md");
    let body = fs::read_to_string(&nested).unwrap();
    assert!(body.starts_with("---\ndescription: "), "{body}");
    assert!(body.contains("## 하는 일"), "{body}");

    let scoped = add(
        &profile,
        &["skill", "triage", "--agent", "main/support/triage"],
    );
    assert!(scoped.status.success(), "{}", text(&scoped));
    let skill = fs::read_to_string(
        profile.join("agents/main/agents/support/agents/triage/skills/triage/SKILL.md"),
    )
    .unwrap();
    assert!(
        skill.starts_with("---\nname: \"triage\"\ndescription: "),
        "{skill}"
    );
    let shared = add(&profile, &["skill", "checklist"]);
    assert!(shared.status.success(), "{}", text(&shared));
    assert!(profile.join("skills/checklist/SKILL.md").is_file());
    assert!(text(&shared).contains("권한 상한은 자동으로 바꾸지 않아요"));

    // 만든 것은 도구·권한을 더하지 않는다
    assert!(!profile.join("skills/checklist/tools.yaml").exists());
    assert_eq!(
        fs::read(profile.join("profile.yaml")).unwrap(),
        yaml_before,
        "profile.yaml은 그대로다"
    );
    let checked = validate(work.path(), "demo");
    assert!(checked.status.success(), "{}", text(&checked));
    assert!(
        text(&checked).contains("에이전트 6 도구 0 트리거 0 바인딩 1"),
        "{}",
        text(&checked)
    );
}

#[test]
fn add_refuses_what_would_overwrite_or_not_validate() {
    let work = tempfile::tempdir().unwrap();
    assert!(init(work.path(), "demo", "team").status.success());
    let profile = profile_dir(work.path(), "demo");
    let before = snapshot(&profile);
    let cases: &[(&[&str], &str)] = &[
        (&["agent", "main/pm"], "이미 있어요"),
        (&["agent", "main"], "이미 있어요"),
        (&["agent", "other"], "루트 에이전트는 하나뿐"),
        (&["agent", "main/nope/deeper"], "상위 에이전트"),
        (&["agent", "Main/cs"], "이름 규칙"),
        (&["agent", "main/../x"], "이름 규칙"),
        (&["agent", "main//x"], "이름 규칙"),
        (&["agent", "main/cs/"], "이름 규칙"),
        (&["agent", "a/b/c/d/e/f/g/h/i"], "단계까지"),
        (
            &["skill", "report-format", "--agent", "main/pm"],
            "같은 이름의 스킬",
        ),
        (&["skill", "delegation"], "같은 이름의 스킬"),
        (
            &["skill", "design-review", "--agent", "main/ghost"],
            "없어요",
        ),
        (&["skill", "Bad_Name"], "이름 규칙"),
        (&["skill", "ok", "--agent", "main/../x"], "이름 규칙"),
    ];
    for (args, expected) in cases {
        let out = add(&profile, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(text(&out).contains(expected), "{args:?}: {}", text(&out));
    }
    assert_eq!(
        snapshot(&profile),
        before,
        "거절한 명령은 아무것도 바꾸지 않는다"
    );
    // 프로필 폴더가 아니면 시작하지 않는다
    let empty = tempfile::tempdir().unwrap();
    let out = add(empty.path(), &["agent", "main"]);
    assert!(
        text(&out).contains("프로필 폴더가 아니에요"),
        "{}",
        text(&out)
    );
    assert!(snapshot(empty.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn links_are_never_followed() {
    use std::os::unix::fs::symlink;
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    assert!(init(work.path(), "demo", "personal").status.success());
    let profile = profile_dir(work.path(), "demo");
    // 이미 있는 부모가 링크: 에이전트 폴더 · 프로필 스킬 폴더
    symlink(outside.path(), profile.join("agents/main/agents")).unwrap();
    symlink(outside.path(), profile.join("skills")).unwrap();
    for args in [&["agent", "main/cs"][..], &["skill", "tidy"][..]] {
        let out = add(&profile, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(text(&out).contains("링크"), "{args:?}: {}", text(&out));
    }
    // init 대상 이름이 (끊어진 것을 포함한) 링크여도 따라가 쓰지 않는다
    symlink(outside.path(), work.path().join("via-link")).unwrap();
    symlink(outside.path().join("nowhere"), work.path().join("dangling")).unwrap();
    for name in ["via-link", "dangling"] {
        let out = init(work.path(), name, "personal");
        assert_eq!(out.status.code(), Some(1), "{name}: {}", text(&out));
        assert!(text(&out).contains("이미 있어요"), "{name}: {}", text(&out));
    }
    assert!(
        fs::read_dir(outside.path()).unwrap().next().is_none(),
        "링크 바깥에는 아무것도 만들지 않는다"
    );
}

/// 막음 7: 이름 규칙에 맞지만 YAML이 글로 읽지 않는 이름(`null`·`true`…)도 만들자마자 검증을 통과한다.
#[test]
fn names_that_yaml_does_not_read_as_text_still_validate_right_after_init() {
    for name in ["null", "true", "off"] {
        for template in ["personal", "team", "company"] {
            let work = tempfile::tempdir().unwrap();
            let made = init(work.path(), name, template);
            assert!(made.status.success(), "{name} {template}: {}", text(&made));
            let checked = validate(work.path(), name);
            assert!(
                checked.status.success(),
                "{name} {template}: {}",
                text(&checked)
            );
            let yaml =
                fs::read_to_string(profile_dir(work.path(), name).join("profile.yaml")).unwrap();
            assert!(yaml.contains(&format!("id: \"{name}\"")), "{yaml}");
        }
    }
}

/// 막음 1: 검증은 `$TMPDIR` 아래 우리 소유 임시 사본에서 하고 끝나면 치운다 — 성공해도 사용자 폴더에 임시 폴더가 남지 않고 `$TMPDIR`도 비어 있다.
/// 쓸 수 없는 `$TMPDIR`는 사용자 폴더에 아무것도 만들기 전에 알린다.
#[test]
fn init_validates_a_private_copy_under_tmpdir_and_leaves_nothing_behind() {
    let work = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    for template in ["personal", "team", "company"] {
        let name = format!("copy-{template}");
        let made = bot(
            &[("TMPDIR", s(tmp.path()))],
            &[
                "init",
                &name,
                "--template",
                template,
                "--dir",
                s(work.path()),
            ],
        );
        assert!(made.status.success(), "{template}: {}", text(&made));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0, "{template}");
    }
    assert_eq!(fs::read_dir(work.path()).unwrap().count(), 3);
    let gone = tmp.path().join("not-here");
    let out = bot(
        &[("TMPDIR", s(&gone))],
        &["init", "blocked", "--dir", s(work.path())],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("$TMPDIR"), "{}", text(&out));
    assert!(!work.path().join("blocked").exists());
    assert_eq!(
        fs::read_dir(work.path()).unwrap().count(),
        3,
        "아무것도 만들지 않았다"
    );
}

#[test]
fn scaffolded_skills_validate_and_reach_model_prompts_without_front_matter() {
    use bot_core::{
        command::{JsonPayload, Target, Work},
        id::{AgentId, Attempt, BindingId, ClaimEpoch, Generation, PrincipalId, ProfileId, RunId},
        state::{Conversation, ConversationKind, RunContext, RunOrigin},
    };
    use bot_profile::{HostCatalog, Limits, load};
    use bot_runtime::context::{ContextInput, assemble};
    use std::{
        collections::BTreeSet,
        time::{Duration, UNIX_EPOCH},
    };

    let work = tempfile::tempdir().unwrap();
    assert!(init(work.path(), "demo", "team").status.success());
    let profile = profile_dir(work.path(), "demo");
    for args in [
        &["skill", "checklist"][..],
        &["skill", "triage", "--agent", "main/developer"][..],
        // YAML 키워드와 같은 이름도 따옴표 처리돼 검증을 통과한다
        &["skill", "null", "--agent", "main/developer"][..],
    ] {
        let out = add(&profile, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        assert!(
            text(&out).contains("본문만 전달돼요"),
            "{args:?}: {}",
            text(&out)
        );
    }
    let checked = validate(work.path(), "demo");
    assert!(checked.status.success(), "{}", text(&checked));

    let mut host = HostCatalog::default();
    host.models
        .insert("demo".into(), BTreeSet::from(["assistant".into()]));
    host.cli_accounts.insert("owner".into());
    let set = load(
        &work.path().join("demo/profiles"),
        &host,
        &Limits::default(),
    )
    .unwrap_or_else(|r| panic!("스캐폴드 프로필 검증 실패:\n{r}"));
    let snap = set.profile(&ProfileId("demo".into())).unwrap();
    let main = &snap.agents[&AgentId("main".into())];
    let dev = &snap.agents[&AgentId("main/developer".into())];

    for (agent_skills, name, heading) in [
        (&main.skills, "report-format", "# 보고 형식"),
        (&main.skills, "delegation", "# 위임: 누구에게 맡길까"),
        (&main.skills, "checklist", "# checklist"),
        (&dev.skills, "triage", "# triage"),
        (&dev.skills, "null", "# null"),
    ] {
        let sk = &agent_skills[name];
        assert!(sk.description.is_some(), "{name}: description 보존");
        assert!(
            sk.instructions.text.starts_with(heading),
            "{name}: {}",
            sk.instructions.text
        );
        assert!(
            !sk.instructions.text.starts_with("---")
                && !sk.instructions.text.contains("description:"),
            "{name}: front matter가 본문에 남음: {}",
            sk.instructions.text
        );
    }

    let run = |agent_id: &str| RunContext {
        attempt: Attempt {
            profile: ProfileId("demo".into()),
            run: RunId("r1".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        root: RunId("r1".into()),
        principal: PrincipalId("local:owner".into()),
        agent: AgentId(agent_id.into()),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: None,
        },
        binding: BindingId("desk".into()),
        api: None,
        scopes: BTreeSet::from(["run".into()]),
        private_memory_read: false,
        work: Work {
            revision: snap.revision.clone(),
            target: Target::Agent {
                agent: AgentId(agent_id.into()),
                instruction: "요약해 줘".into(),
            },
            input: JsonPayload(r#"{"text":"요약해 줘"}"#.into()),
            attachments: vec![],
            steps: vec![],
            pass_pos: None,
        },
        approver: None,
        trigger: None,
        origin: RunOrigin::Message,
    };

    for (agent_id, expected_headings) in [
        (
            "main",
            &[
                "### report-format\n# 보고 형식",
                "### delegation\n# 위임: 누구에게 맡길까",
                "### checklist\n# checklist",
            ][..],
        ),
        (
            "main/developer",
            &[
                "### report-format\n# 보고 형식",
                "### triage\n# triage",
                "### null\n# null",
            ][..],
        ),
    ] {
        let assembled = assemble(&ContextInput {
            snapshot: snap,
            run: &run(agent_id),
            history: &[],
            memory: &[],
            now: UNIX_EPOCH + Duration::from_secs(1_795_487_400),
        })
        .unwrap();
        for heading in expected_headings {
            assert!(
                assembled.system.contains(heading),
                "{agent_id}: {heading} 없음\n{}",
                assembled.system
            );
        }
        for leaked in [
            "---\nname:",
            "name: report-format",
            "name: delegation",
            "name: \"checklist\"",
            "name: \"triage\"",
            "name: \"null\"",
            "description:",
        ] {
            assert!(
                !assembled.system.contains(leaked),
                "{agent_id}: front matter `{leaked}`가 모델 프롬프트에 노출됨\n{}",
                assembled.system
            );
        }
    }
}

#[test]
fn add_skill_creator_writes_ready_to_use_authoring_skill_that_validates() {
    let work = tempfile::tempdir().unwrap();
    assert!(init(work.path(), "demo", "team").status.success());
    let profile = profile_dir(work.path(), "demo");

    let out = add(&profile, &["skill", "skill-creator", "--agent", "main"]);
    assert!(out.status.success(), "{}", text(&out));
    let skill_path = profile.join("agents/main/skills/skill-creator/SKILL.md");
    assert!(skill_path.is_file());
    let body = fs::read_to_string(&skill_path).unwrap();
    assert!(
        body.starts_with("---\nname: \"skill-creator\"\ndescription: "),
        "{body}"
    );
    for needle in [
        "skill.inspect",
        "skill.create",
        "skill.update",
        "skill.delete",
        "mcp.add",
        "mcp.remove",
        "SOPS",
    ] {
        assert!(
            body.contains(needle),
            "skill-creator에 `{needle}` 안내가 없어요"
        );
    }

    let checked = validate(work.path(), "demo");
    assert!(checked.status.success(), "{}", text(&checked));
}
