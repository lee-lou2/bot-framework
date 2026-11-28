//! Process-level tests for `bot init` and `bot add`: immediate validation across all templates, no overwrites,
//! symlink refusal, untouched permission ceilings in `profile.yaml`, and front-matter stripping in assembled prompts.
use crate::process_support::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn lower(out: &std::process::Output) -> String {
    text(out).to_ascii_lowercase()
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

/// Captures relative paths and file bytes under `root` without following symlinks.
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
        // Scaffolding commands must ignore invalid BOT_* env vars.
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
        let first_line = shown.lines().next().unwrap_or("");
        assert!(
            first_line.contains(template)
                && first_line.contains(&agents.to_string())
                && first_line.to_ascii_lowercase().contains("agent"),
            "{shown}"
        );
        assert!(shown.contains("export BOT_PROFILES="), "{shown}");
        assert_eq!(
            fs::read_dir(work.path()).unwrap().count(),
            1,
            "staging directory must not remain"
        );
        let checked = validate(work.path(), "demo-bot");
        assert!(checked.status.success(), "{template}: {}", text(&checked));
        let report = text(&checked);
        assert!(
            report.contains("Validation passed: 1 profile(s)"),
            "{report}"
        );
        assert!(
            report.contains(&format!("agents {agents} tools 0 triggers 0 bindings 1")),
            "{template}: {report}"
        );
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
        assert!(lower(&out).contains("exists"), "{}", text(&out));
    }
    assert_eq!(
        snapshot(work.path()),
        before,
        "existing files and directories must remain untouched"
    );
    assert!(init(work.path(), "first", "team").status.success());
    let first = snapshot(work.path());
    assert!(!init(work.path(), "first", "personal").status.success());
    assert_eq!(snapshot(work.path()), first);

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
        assert!(lower(&out).contains("lowercase"), "{bad:?}: {}", text(&out));
    }
    assert!(snapshot(&inner).is_empty());
    assert!(!work.path().join("evil").exists());

    let missing = work.path().join("not-here");
    let out = init(&missing, "x", "personal");
    assert_eq!(out.status.code(), Some(1));
    let err = lower(&out);
    assert!(
        err.contains("does not exist") || err.contains("not found") || err.contains("missing"),
        "{err}"
    );
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
        "permissions should only be documented in guidance: {}",
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
    assert!(body.contains("\n## ") && body.contains("triage"), "{body}");

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
    let shared_out = text(&shared);
    let shared_low = shared_out.to_ascii_lowercase();
    assert!(
        shared_out.contains("profile.yaml")
            && (shared_low.contains("permission")
                || shared_low.contains("ceiling")
                || shared_low.contains("policy")),
        "{shared_out}"
    );

    assert!(!profile.join("skills/checklist/tools.yaml").exists());
    assert_eq!(
        fs::read(profile.join("profile.yaml")).unwrap(),
        yaml_before,
        "profile.yaml must remain unchanged"
    );
    let checked = validate(work.path(), "demo");
    assert!(checked.status.success(), "{}", text(&checked));
    assert!(
        text(&checked).contains("agents 6 tools 0 triggers 0 bindings 1"),
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
        (&["agent", "main/pm"], "exists"),
        (&["agent", "main"], "exists"),
        (&["agent", "other"], "root"),
        (&["agent", "main/nope/deeper"], "main/nope"),
        (&["agent", "Main/cs"], "lowercase"),
        (&["agent", "main/../x"], "lowercase"),
        (&["agent", "main//x"], "lowercase"),
        (&["agent", "main/cs/"], "lowercase"),
        (&["agent", "a/b/c/d/e/f/g/h/i"], "8"),
        (
            &["skill", "report-format", "--agent", "main/pm"],
            "skills/report-format",
        ),
        (&["skill", "delegation"], "agents/main/skills/delegation"),
        (
            &["skill", "design-review", "--agent", "main/ghost"],
            "main/ghost",
        ),
        (&["skill", "Bad_Name"], "lowercase"),
        (&["skill", "ok", "--agent", "main/../x"], "lowercase"),
    ];
    for (args, expected) in cases {
        let out = add(&profile, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(lower(&out).contains(expected), "{args:?}: {}", text(&out));
    }
    assert_eq!(
        snapshot(&profile),
        before,
        "refused commands must leave the profile unchanged"
    );
    let empty = tempfile::tempdir().unwrap();
    let out = add(empty.path(), &["agent", "main"]);
    assert!(text(&out).contains("profile.yaml"), "{}", text(&out));
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
    symlink(outside.path(), profile.join("agents/main/agents")).unwrap();
    symlink(outside.path(), profile.join("skills")).unwrap();
    for args in [&["agent", "main/cs"][..], &["skill", "tidy"][..]] {
        let out = add(&profile, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", text(&out));
        assert!(lower(&out).contains("link"), "{args:?}: {}", text(&out));
    }
    symlink(outside.path(), work.path().join("via-link")).unwrap();
    symlink(outside.path().join("nowhere"), work.path().join("dangling")).unwrap();
    for name in ["via-link", "dangling"] {
        let out = init(work.path(), name, "personal");
        assert_eq!(out.status.code(), Some(1), "{name}: {}", text(&out));
        assert!(lower(&out).contains("exists"), "{name}: {}", text(&out));
    }
    assert!(
        fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing may be created outside the target via symlinks"
    );
}

/// YAML-keyword profile names (`null`, `true`, `off`) must be quoted so they validate as strings.
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

/// Verification runs on an owned staging copy under `$TMPDIR` and cleans it up; an unwritable `$TMPDIR` fails cleanly.
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
        "nothing may be created on $TMPDIR failure"
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
        &["skill", "null", "--agent", "main/developer"][..],
    ] {
        let out = add(&profile, args);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        let out_text = text(&out);
        let low = out_text.to_ascii_lowercase();
        assert!(
            (low.contains("front matter") || low.contains("frontmatter")) && low.contains("body"),
            "{args:?}: {out_text}"
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
    .unwrap_or_else(|r| panic!("scaffolded profile failed validation:\n{r}"));
    let snap = set.profile(&ProfileId("demo".into())).unwrap();
    let main = &snap.agents[&AgentId("main".into())];
    let dev = &snap.agents[&AgentId("main/developer".into())];

    for (agent_skills, name, expected_word) in [
        (&main.skills, "report-format", "report"),
        (&main.skills, "delegation", "delegat"),
        (&main.skills, "checklist", "# checklist"),
        (&dev.skills, "triage", "# triage"),
        (&dev.skills, "null", "# null"),
    ] {
        let sk = &agent_skills[name];
        assert!(sk.description.is_some(), "{name}: description preserved");
        let first_line = sk.instructions.text.lines().next().unwrap_or("");
        assert!(
            first_line.starts_with("# ") && first_line.to_ascii_lowercase().contains(expected_word),
            "{name}: {}",
            sk.instructions.text
        );
        assert!(
            !sk.instructions.text.starts_with("---")
                && !sk.instructions.text.contains("description:"),
            "{name}: front matter leaked into body: {}",
            sk.instructions.text
        );
    }

    let heading = |name: &str| main.skills[name].instructions.text.lines().next().unwrap();
    let report_section = format!("### report-format\n{}", heading("report-format"));
    let delegation_section = format!("### delegation\n{}", heading("delegation"));

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
        intake: bot_core::state::IntakeMeta::default(),
        scopes: BTreeSet::from(["run".into()]),
        private_memory_read: false,
        work: Work {
            revision: snap.revision.clone(),
            target: Target::Agent {
                agent: AgentId(agent_id.into()),
                instruction: "Summarize this".into(),
            },
            input: JsonPayload(r#"{"text":"Summarize this"}"#.into()),
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
            vec![
                report_section.as_str(),
                delegation_section.as_str(),
                "### checklist\n# checklist",
            ],
        ),
        (
            "main/developer",
            vec![
                report_section.as_str(),
                "### triage\n# triage",
                "### null\n# null",
            ],
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
                "{agent_id}: missing {heading}\n{}",
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
                "{agent_id}: front matter `{leaked}` leaked into model prompt\n{}",
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
            "skill-creator missing `{needle}` guidance"
        );
    }

    let checked = validate(work.path(), "demo");
    assert!(checked.status.success(), "{}", text(&checked));
}
