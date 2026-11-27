//! `bot mcp sync`가 대상 프로필을 정하는 길(`locate_skill`)과 쓰기 전의 잠금 검증(`verify_lock`): `validate`와 같은 로더 규칙을 따른다.

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
    let found = locate(&root, &skill_dir(&root)).expect("찾는다");
    assert_eq!(found.profile.0, "personal");
    assert_eq!(found.skill, SKILL);
    assert_eq!(found.name, "agenda");
    assert!(String::from_utf8_lossy(&found.tools_yaml).contains("server: demo-agenda"));
    // 프로필 수준 스킬도 같다
    let files = edit(base(), "skills/shared/SKILL.md", "", "공용\n");
    let files = edit(
        files,
        "skills/shared/tools.yaml",
        "",
        "version: 1\nserver: demo-agenda\ntools: []\n",
    );
    let root = setup(&files);
    let found =
        locate(&root, &root.path().join("personal/skills/shared")).expect("프로필 수준 스킬");
    assert_eq!(found.skill, "skills/shared");
    // 심볼릭 루트(루트 자체가 링크)는 허용하고, 링크를 거친 경로와 실제 경로가 같은 곳을 가리킨다.
    // 링크 경로는 정규 루트를 지나 `..`로 나온다: 임시 폴더가 링크 밑인지(macOS)와 상관없이 같은 경우를 본다
    let real = std::fs::canonicalize(root.path()).unwrap();
    let link = real
        .join("..")
        .join(format!("loc-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let via = locate_skill(&link, &link.join("personal/skills/shared"), &LIMITS());
    let _ = std::fs::remove_file(&link);
    assert_eq!(via.expect("링크된 루트").skill, "skills/shared");
}

#[test]
fn nothing_is_located_when_the_profile_would_not_load_like_validate() {
    type Spoil = fn(Files) -> Files;
    let cases: Vec<(&str, Spoil)> = vec![
        ("id 없음", |f| {
            edit(f, "profile.yaml", "id: personal\n", "")
        }),
        ("폴더와 다른 id", |f| {
            edit(f, "profile.yaml", "id: personal\n", "id: other\n")
        }),
        ("스키마에 어긋남", |f| {
            edit(f, "profile.yaml", "model: assistant\n", "")
        }),
        ("profile.yaml 없음", |f| {
            edit(f, "profile.yaml", crate::support::DELETE, "")
        }),
        ("모르는 파일", |f| edit(f, "stray.txt", "", "x")),
        ("루트 에이전트 없음", |f| {
            edit(f, "agents/main/AGENT.md", crate::support::DELETE, "")
        }),
        ("SKILL.md 없음", |f| {
            edit(f, &format!("{SKILL}/SKILL.md"), crate::support::DELETE, "")
        }),
        ("tools.yaml 없음", |f| {
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
        ("단일 32 KiB 초과", over.as_str(), "skill-too-large"),
        (
            "allowed-tools 금지",
            "---\nname: agenda\nallowed-tools: Bash\n---\n일정\n",
            "skill-allowed-tools",
        ),
        (
            "미닫힘 front matter",
            "---\nname: agenda\n일정\n",
            "front-matter-unclosed",
        ),
        ("단독 --- 파일 끝", "---", "front-matter-unclosed"),
        (
            "폴더명과 다른 name",
            "---\nname: other\n---\n일정\n",
            "skill-name-mismatch",
        ),
        (
            "미지 키",
            "---\nname: agenda\nextra: 1\n---\n일정\n",
            "schema",
        ),
        (
            "front matter 뒤 빈 본문",
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
    // 정상 front matter를 가진 SKILL.md는 문제없이 확정한다
    let valid = "---\nname: agenda\ndescription: 일정 조회\n---\n일정을 조회한다.\n";
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
    // 대조: 지금은 찾는다
    assert!(locate(&root, &dir).is_ok());
    // 프로필 밖·루트 자신·프로필 폴더·스킬이 아닌 폴더·`..`로 돌아 나온 경로
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
    // 프로필 안의 링크는 어디에 있든 프로필을 읽을 수 없게 한다(validate와 같다)
    std::os::unix::fs::symlink("/etc/hosts", profile.join("knowledge/linked.md")).unwrap();
    assert!(locate(&root, &dir).is_err());
    std::fs::remove_file(profile.join("knowledge/linked.md")).unwrap();
    assert!(locate(&root, &dir).is_ok());
    // 하드링크
    std::fs::hard_link(profile.join("rules.md"), profile.join("knowledge/hard.md")).unwrap();
    assert!(locate(&root, &dir).is_err());
    std::fs::remove_file(profile.join("knowledge/hard.md")).unwrap();
    // 스킬 폴더가 링크이거나 링크를 거친 경로: 실제 위치가 아니라 링크 자체를 거절한다
    let moved = profile.join("agents/main/skills/real");
    std::fs::rename(&dir, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &dir).unwrap();
    assert!(locate(&root, &dir).is_err());
    assert!(
        locate(&root, &moved).is_err(),
        "링크가 든 프로필은 읽지 않는다"
    );
    std::fs::remove_file(&dir).unwrap();
    std::fs::rename(&moved, &dir).unwrap();
    // 루트 아래의 다른 프로필 폴더가 링크이거나 루트에 낱개 파일이 있으면 루트 목록이 거절한다
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
    // `list`는 `input`을 생략한 원격 도구다
    let tools = "version: 1\nserver: demo-agenda\ntools:\n  - id: list\n    class: read\n    scopes: [calendar.read]\n";
    let files = edit(
        base(),
        &format!("{SKILL}/tools.yaml"),
        crate::support::DELETE,
        "",
    );
    let files = edit(files, &format!("{SKILL}/tools.yaml"), "", tools);
    let root = setup(&files);
    let found = locate(&root, &skill_dir(&root)).expect("찾는다");
    let render = |description: &str| lock::render("demo-agenda", &entries(description)).unwrap();
    let limits = LIMITS();
    // 정상
    found
        .verify_lock(render("목록").as_bytes(), &limits)
        .expect("통과");
    // YAML 값 하나의 한도(64 KiB)를 넘는 설명, 파일 한도(256 KiB)를 넘는 잠금
    let long = render(&"a".repeat(65_537));
    let issues = found
        .verify_lock(long.as_bytes(), &limits)
        .expect_err("값이 너무 길어요");
    assert!(
        issues.iter().any(|i| i.code == "yaml-too-large"),
        "{issues:?}"
    );
    let huge = format!("{}\n# {}\n", render("x"), "b".repeat(300_000));
    let issues = found
        .verify_lock(huge.as_bytes(), &limits)
        .expect_err("파일이 너무 커요");
    assert!(
        issues.iter().any(|i| i.code == "yaml-too-large"),
        "{issues:?}"
    );
    // 손으로 고친 해시·다른 서버의 잠금·tools.yaml이 정한 이름과 다른 잠금
    let tampered = render("목록").replace("목록", "다른 설명");
    assert!(found.verify_lock(tampered.as_bytes(), &limits).is_err());
    let foreign = lock::render("elsewhere", &entries("목록")).unwrap();
    assert!(found.verify_lock(foreign.as_bytes(), &limits).is_err());
    let renamed = render("목록").replace("name: \"list\"", "name: \"list_v2\"");
    assert!(found.verify_lock(renamed.as_bytes(), &limits).is_err());
    // 금지된 보이지 않는 글자(방향 전환 문자)가 든 설명은 프로필 로더가 거절하므로 쓰지 않는다
    let bidi = render("목록 \u{202e}evil");
    assert!(found.verify_lock(bidi.as_bytes(), &limits).is_err());
    // SKILL.md 본문 상한과 allowed-tools 금지 역시 동기화 검증에서 그대로 집행한다
    let ok_lock = render("목록");
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
    let bad_fm = b"---\nname: agenda\nallowed-tools: Bash\n---\n\xec\x9d\xbc\xec\xa0\x95\n";
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
