//! Tests for `locate_skill` and `verify_lock` used by `bot mcp sync` under the same loader rules as `validate`.

use crate::support::{Files, Temp, base, edit, write_profile};
use bot_profile::{
    Limits,
    locate::{Located, locate_skill},
    lock::{self, Entry},
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

const SKILL: &str = "agents/main/skills/agenda";
const LIMITS: fn() -> Limits = Limits::default;

fn setup(files: &Files) -> Temp {
    let root = Temp::new("locate");
    write_profile(root.path(), "personal", files);
    root
}

fn locate(root: &Temp, dir: &Path) -> Result<Located, Vec<String>> {
    locate_skill(root.path(), dir, &LIMITS())
        .map_err(|issues| issues.iter().map(|issue| issue.code.to_owned()).collect())
}

fn skill_dir(root: &Temp) -> std::path::PathBuf {
    root.path().join("personal").join(SKILL)
}

#[test]
fn a_skill_folder_of_a_valid_profile_is_located_with_its_bytes() {
    let root = setup(&base());
    let found = locate(&root, &skill_dir(&root)).expect("located");
    assert_eq!(found.profile.0, "personal");
    assert_eq!(found.skill, SKILL);
    assert_eq!(found.name, "agenda");
    assert!(String::from_utf8_lossy(&found.tools_yaml).contains("server: demo-agenda"));
    let files = edit(base(), "skills/shared/SKILL.md", "", "Shared skill.\n");
    let files = edit(
        files,
        "skills/shared/tools.yaml",
        "",
        "version: 1\nserver: demo-agenda\ntools: []\n",
    );
    let root = setup(&files);
    let found =
        locate(&root, &root.path().join("personal/skills/shared")).expect("profile-level skill");
    assert_eq!(found.skill, "skills/shared");
    // A symlinked profiles root itself is allowed; place the symlink beside canonical root to stay consistent across macOS /var -> /private/var.
    let real = std::fs::canonicalize(root.path()).unwrap();
    let link = real
        .join("..")
        .join(format!("loc-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let via = locate_skill(&link, &link.join("personal/skills/shared"), &LIMITS());
    let _ = std::fs::remove_file(&link);
    assert_eq!(via.expect("symlinked root").skill, "skills/shared");
}

#[test]
fn nothing_is_located_when_the_profile_would_not_load_like_validate() {
    type Spoil = fn(Files) -> Files;
    let cases: Vec<(&str, Spoil)> = vec![
        ("missing id", |f| {
            edit(f, "profile.yaml", "id: personal\n", "")
        }),
        ("id mismatch", |f| {
            edit(f, "profile.yaml", "id: personal\n", "id: other\n")
        }),
        ("schema error", |f| {
            edit(f, "profile.yaml", "model: assistant\n", "")
        }),
        ("missing profile.yaml", |f| {
            edit(f, "profile.yaml", crate::support::DELETE, "")
        }),
        ("stray file", |f| edit(f, "stray.txt", "", "x")),
        ("missing root agent", |f| {
            edit(f, "agents/main/AGENT.md", crate::support::DELETE, "")
        }),
        ("missing SKILL.md", |f| {
            edit(f, &format!("{SKILL}/SKILL.md"), crate::support::DELETE, "")
        }),
        ("missing tools.yaml", |f| {
            edit(
                f,
                &format!("{SKILL}/tools.yaml"),
                crate::support::DELETE,
                "",
            )
        }),
    ];
    for (label, spoil) in cases {
        let root = setup(&spoil(base()));
        let dir = skill_dir(&root);
        assert!(locate(&root, &dir).is_err(), "{label}");
    }
}

#[test]
fn invalid_target_skill_md_is_refused_before_location_succeeds() {
    let over = "a".repeat(bot_profile::SKILL_MAX_BYTES + 1);
    let cases = [
        ("over 64 KiB", over.as_str(), "skill-too-large"),
        (
            "forbidden allowed-tools",
            "---\nname: agenda\nallowed-tools: Bash\n---\nSchedule\n",
            "skill-allowed-tools",
        ),
        (
            "unclosed front matter",
            "---\nname: agenda\nSchedule\n",
            "front-matter-unclosed",
        ),
        ("bare --- at EOF", "---", "front-matter-unclosed"),
        (
            "name mismatch",
            "---\nname: other\n---\nSchedule\n",
            "skill-name-mismatch",
        ),
        (
            "unknown key",
            "---\nname: agenda\nextra: 1\n---\nSchedule\n",
            "schema",
        ),
        (
            "empty body after front matter",
            "---\nname: agenda\n---\n",
            "empty-file",
        ),
    ];
    for (label, content, want) in cases {
        let files = edit(
            base(),
            &format!("{SKILL}/SKILL.md"),
            crate::support::DELETE,
            "",
        );
        let files = edit(files, &format!("{SKILL}/SKILL.md"), "", content);
        let root = setup(&files);
        let codes = locate(&root, &skill_dir(&root)).expect_err(label);
        assert!(codes.iter().any(|c| c == want), "{label}: {codes:?}");
    }
    let valid = "---\nname: agenda\ndescription: Schedule lookup\n---\nCheck the schedule.\n";
    let files = edit(
        base(),
        &format!("{SKILL}/SKILL.md"),
        crate::support::DELETE,
        "",
    );
    let files = edit(files, &format!("{SKILL}/SKILL.md"), "", valid);
    let root = setup(&files);
    assert!(locate(&root, &skill_dir(&root)).is_ok());
}

#[test]
fn links_special_files_and_paths_outside_the_tree_are_never_followed() {
    let root = setup(&base());
    let dir = skill_dir(&root);
    assert!(locate(&root, &dir).is_ok());
    let profile = root.path().join("personal");
    for not_a_skill in [
        root.path().to_owned(),
        profile.clone(),
        profile.join("agents/main"),
        profile.join("knowledge"),
        profile.join("agents/main/skills"),
        root.path().join("elsewhere"),
        dir.join("../agenda"),
        dir.join("../../../../.."),
    ] {
        assert!(
            locate(&root, &not_a_skill).is_err(),
            "{}",
            not_a_skill.display()
        );
    }
    std::os::unix::fs::symlink("/etc/hosts", profile.join("knowledge/linked.md")).unwrap();
    assert!(locate(&root, &dir).is_err());
    std::fs::remove_file(profile.join("knowledge/linked.md")).unwrap();
    assert!(locate(&root, &dir).is_ok());
    std::fs::hard_link(profile.join("rules.md"), profile.join("knowledge/hard.md")).unwrap();
    assert!(locate(&root, &dir).is_err());
    std::fs::remove_file(profile.join("knowledge/hard.md")).unwrap();
    let moved = profile.join("agents/main/skills/real");
    std::fs::rename(&dir, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &dir).unwrap();
    assert!(locate(&root, &dir).is_err());
    assert!(
        locate(&root, &moved).is_err(),
        "profile containing any symlink must be rejected"
    );
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&moved, &dir).unwrap();
    std::os::unix::fs::symlink(&profile, root.path().join("alias")).unwrap();
    assert!(locate(&root, &dir).is_err());
}

fn entries(description: &str) -> BTreeMap<String, Entry> {
    let schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
    let sha = bot_core::mcp::definition_sha256("list", description, &schema).unwrap();
    [(
        "list".to_owned(),
        Entry {
            name: "list".into(),
            description: description.into(),
            input_schema: schema,
            sha256: sha,
        },
    )]
    .into()
}

#[test]
fn a_lock_is_verified_like_validate_before_it_may_be_written() {
    let tools = "version: 1\nserver: demo-agenda\ntools:\n  - id: list\n    class: read\n    scopes: [calendar.read]\n";
    let files = edit(
        base(),
        &format!("{SKILL}/tools.yaml"),
        crate::support::DELETE,
        "",
    );
    let files = edit(files, &format!("{SKILL}/tools.yaml"), "", tools);
    let root = setup(&files);
    let found = locate(&root, &skill_dir(&root)).expect("located");
    let render = |description: &str| lock::render("demo-agenda", &entries(description)).unwrap();
    let limits = LIMITS();
    found
        .verify_lock(render("List items").as_bytes(), &limits)
        .expect("valid lock");
    let long = render(&"a".repeat(65_537));
    let issues = found
        .verify_lock(long.as_bytes(), &limits)
        .expect_err("scalar too large");
    assert!(
        issues.iter().any(|i| i.code == "yaml-too-large"),
        "{issues:?}"
    );
    let huge = format!("{}\n# {}\n", render("x"), "b".repeat(300_000));
    let issues = found
        .verify_lock(huge.as_bytes(), &limits)
        .expect_err("file too large");
    assert!(
        issues.iter().any(|i| i.code == "yaml-too-large"),
        "{issues:?}"
    );
    let tampered = render("List items").replace("List items", "Other description");
    assert!(found.verify_lock(tampered.as_bytes(), &limits).is_err());
    let foreign = lock::render("elsewhere", &entries("List items")).unwrap();
    assert!(found.verify_lock(foreign.as_bytes(), &limits).is_err());
    let renamed = render("List items").replace("name: \"list\"", "name: \"list_v2\"");
    assert!(found.verify_lock(renamed.as_bytes(), &limits).is_err());
    let bidi = render("List \u{202e}evil");
    assert!(found.verify_lock(bidi.as_bytes(), &limits).is_err());
    let ok_lock = render("List items");
    let over = "a".repeat(bot_profile::SKILL_MAX_BYTES + 1);
    let err = bot_profile::locate::check_skill(
        "agenda",
        over.as_bytes(),
        tools.as_bytes(),
        ok_lock.as_bytes(),
        &limits,
    )
    .unwrap_err();
    assert!(err.iter().any(|i| i.code == "skill-too-large"), "{err:?}");
    let bad_fm = b"---\nname: agenda\nallowed-tools: Bash\n---\nSchedule\n";
    let err = bot_profile::locate::check_skill(
        "agenda",
        bad_fm,
        tools.as_bytes(),
        ok_lock.as_bytes(),
        &limits,
    )
    .unwrap_err();
    assert!(
        err.iter().any(|i| i.code == "skill-allowed-tools"),
        "{err:?}"
    );
}
