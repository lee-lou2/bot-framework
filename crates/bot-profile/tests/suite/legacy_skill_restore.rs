//! 구판(`BOTPROFILE1`) 번들 복원과 신판(`BOTPROFILE2`) 판본 분리 회귀 시험:
//! - 구판에서 유효했던 SKILL.md(정상 front matter·닫히지 않은 `---`·단독 `---`·미지 키·`allowed-tools`·이름 불일치·단일 32 KiB 초과·상속 합계 초과)로
//!   만든 `BOTPROFILE1` 번들은 `from_bundle`로 거절 0건 복원되고, 조립된 스킬 지침이 구판(`md.trim_end()`)과 바이트 단위로 같다.
//! - front matter가 있는 같은 폴더를 신판 로더로 읽으면 `BOTPROFILE2`로 묶여 구판과 판본 해시가 다르고, front matter가 제거되며 `description`이 보존된다.
//! - 새 폴더 검증(`load`)과 `BOTPROFILE2` 복원에서는 새 규칙 거절이 그대로 집행된다.
//! - `BOTPROFILE1` 복원에서도 해시·ID·예약 도구·BOM·총량 검사는 그대로 집행된다.

use crate::support::{Files, Temp, raw_bundle, write_profile};
use bot_core::id::{AgentId, ProfileId};
use bot_profile::{
    HostCatalog, Limits, from_bundle, load, read_bundle, read_legacy_bundle, revision_of,
};

const BODY: &str = "Visible stable paragraph.\n";
const VALID_FM: &str =
    "---\nname: memo\ndescription: HEADER_SENTINEL\n---\nVisible stable paragraph.\n";

fn fm(tail: &str) -> String {
    format!("---\n{tail}\n---\n{BODY}")
}

fn minimal_profile(skill_text: &str) -> Files {
    vec![
        (
            "profile.yaml".to_owned(),
            "version: 1\nid: personal\ntimezone: UTC\nmodel: assistant\nroles:\n  owner: [run]\nmembers:\n  - {identity: \"local:owner\", roles: [owner]}\nbindings:\n  - {id: desk, adapter: cli, account: owner, agent: main}\n".to_owned(),
        ),
        ("agents/main/AGENT.md".to_owned(), "Stable agent.\n".to_owned()),
        ("skills/memo/SKILL.md".to_owned(), skill_text.to_owned()),
    ]
}

fn agg_profile() -> Files {
    let mut files = minimal_profile(&"a".repeat(16_384));
    files.push((
        "agents/main/agents/research/AGENT.md".to_owned(),
        "Child agent.\n".to_owned(),
    ));
    files.push((
        "agents/main/skills/other/SKILL.md".to_owned(),
        "b".repeat(16_385),
    ));
    files
}

fn raw_v2_bundle(files: &Files) -> Vec<u8> {
    let mut out = raw_bundle(files);
    out.splice(0..12, *b"BOTPROFILE2\n");
    out
}

#[test]
fn legacy_v1_bundles_restore_every_previously_valid_skill_byte_identically() {
    let id = ProfileId("personal".into());
    let main = AgentId("main".into());
    let limits = Limits::default();
    let big = "x".repeat(32_769);
    let crlf = VALID_FM.replace('\n', "\r\n");
    let unclosed = format!("---\n{BODY}");
    let unknown = fm("retired-example: text");
    let authority = fm("allowed-tools: noop");
    let wrongname = fm("name: elsewhere");
    let agg_memo = "a".repeat(16_384);
    let cases: Vec<(&str, Files, String)> = vec![
        ("clean", minimal_profile(BODY), BODY.trim_end().to_owned()),
        (
            "valid",
            minimal_profile(VALID_FM),
            VALID_FM.trim_end().to_owned(),
        ),
        (
            "validcrlf",
            minimal_profile(&crlf),
            crlf.trim_end().to_owned(),
        ),
        (
            "unclosed",
            minimal_profile(&unclosed),
            unclosed.trim_end().to_owned(),
        ),
        (
            "unknown",
            minimal_profile(&unknown),
            unknown.trim_end().to_owned(),
        ),
        (
            "authority",
            minimal_profile(&authority),
            authority.trim_end().to_owned(),
        ),
        (
            "wrongname",
            minimal_profile(&wrongname),
            wrongname.trim_end().to_owned(),
        ),
        ("big", minimal_profile(&big), big),
        ("agg", agg_profile(), agg_memo),
        ("bare", minimal_profile("---"), "---".to_owned()),
        (
            "agent-bare",
            minimal_profile(BODY)
                .into_iter()
                .map(|(p, c)| {
                    if p == "agents/main/AGENT.md" {
                        (p, "---".to_owned())
                    } else {
                        (p, c)
                    }
                })
                .collect(),
            BODY.trim_end().to_owned(),
        ),
        (
            "agent-bare-cr",
            minimal_profile(BODY)
                .into_iter()
                .map(|(p, c)| {
                    if p == "agents/main/AGENT.md" {
                        (p, "---\r".to_owned())
                    } else {
                        (p, c)
                    }
                })
                .collect(),
            BODY.trim_end().to_owned(),
        ),
    ];

    for (name, files, expected_memo) in cases {
        let v1_bundle = raw_bundle(&files);
        assert!(v1_bundle.starts_with(b"BOTPROFILE1\n"), "{name}");
        let v1_rev = revision_of(&v1_bundle);
        let restored = from_bundle(&v1_bundle, &id, &v1_rev, &limits)
            .unwrap_or_else(|e| panic!("{name}: 구판 번들 복원 실패: {e:?}"));
        assert_eq!(restored.revision, v1_rev, "{name}");
        let memo = &restored.agents[&main].skills["memo"];
        assert_eq!(
            memo.instructions.text, expected_memo,
            "{name}: 구판 지침과 바이트 단위로 같아야 한다"
        );
        assert_eq!(
            memo.description, None,
            "{name}: 구판 번들은 description 없음"
        );
        assert!(
            memo.tools.is_empty(),
            "{name}: 어떤 헤더도 도구를 추가하지 않는다"
        );

        // `read_legacy_bundle`은 같은 폴더에서 정확히 같은 V1 SHA-256과 바이트를 재생성한다
        let tmp = Temp::new(&format!("legacy-put-{name}"));
        write_profile(tmp.path(), "personal", &files);
        let (put_id, put_rev, put_bytes) =
            read_legacy_bundle(&tmp.path().join("personal"), &limits)
                .unwrap_or_else(|r| panic!("{name}: read_legacy_bundle 실패:\n{r}"));
        assert_eq!(put_id, id, "{name}");
        assert_eq!(put_rev, v1_rev, "{name}");
        assert_eq!(put_bytes, v1_bundle, "{name}");
    }
}

#[test]
fn front_matter_profiles_get_distinct_v2_revision_while_plain_profiles_keep_v1() {
    let id = ProfileId("personal".into());
    let main = AgentId("main".into());
    let limits = Limits::default();

    // front matter가 있는 정상 프로필: 새 로더는 BOTPROFILE2로 묶고 판본이 구판(BOTPROFILE1)과 다르다
    let valid_files = minimal_profile(VALID_FM);
    let v1_bundle = raw_bundle(&valid_files);
    let v1_rev = revision_of(&v1_bundle);
    let tmp = Temp::new("v2-distinct");
    write_profile(tmp.path(), "personal", &valid_files);
    let set = load(tmp.path(), &HostCatalog::demo(), &limits).unwrap();
    let v2_bundle = set.bundle(&id).unwrap();
    let v2_snap = set.profile(&id).unwrap();
    assert!(
        v2_bundle.starts_with(b"BOTPROFILE2\n"),
        "front matter가 있는 프로필은 BOTPROFILE2여야 한다"
    );
    assert_ne!(
        v2_snap.revision, v1_rev,
        "같은 폴더여도 구판 해석(V1)과 신판 해석(V2)은 판본이 달라야 한다"
    );
    let memo = &v2_snap.agents[&main].skills["memo"];
    assert_eq!(memo.description.as_deref(), Some("HEADER_SENTINEL"));
    assert_eq!(memo.instructions.text, "Visible stable paragraph.");
    assert!(!memo.instructions.text.contains("HEADER_SENTINEL"));
    // V2 번들 복원도 신판 해석(본문 제거 + description 보존)을 유지한다
    let restored_v2 = from_bundle(v2_bundle, &id, &v2_snap.revision, &limits).unwrap();
    assert_eq!(format!("{restored_v2:?}"), format!("{:?}", **v2_snap));
    // `read_bundle`도 V2 로더와 같은 바이트·판본을 만든다
    let (_, read_rev, read_bytes) = read_bundle(&tmp.path().join("personal"), &limits).unwrap();
    assert_eq!(read_rev, v2_snap.revision);
    assert_eq!(read_bytes, v2_bundle);

    // description 없이 name만 있는 front matter도 제거됐으므로 반드시 BOTPROFILE2로 구별된다
    let name_only = minimal_profile(&fm("name: memo"));
    let tmp_name = Temp::new("v2-name-only");
    write_profile(tmp_name.path(), "personal", &name_only);
    let set_name = load(tmp_name.path(), &HostCatalog::demo(), &limits).unwrap();
    assert!(set_name.bundle(&id).unwrap().starts_with(b"BOTPROFILE2\n"));
    assert_ne!(
        set_name.profile(&id).unwrap().revision,
        revision_of(&raw_bundle(&name_only))
    );

    // front matter가 없는 평문 프로필은 구판·신판 해석이 바이트 단위로 같고 BOTPROFILE1 판본을 유지한다
    let clean_files = minimal_profile(BODY);
    let tmp_clean = Temp::new("v1-clean");
    write_profile(tmp_clean.path(), "personal", &clean_files);
    let set_clean = load(tmp_clean.path(), &HostCatalog::demo(), &limits).unwrap();
    assert!(set_clean.bundle(&id).unwrap().starts_with(b"BOTPROFILE1\n"));
    assert_eq!(
        set_clean.profile(&id).unwrap().revision,
        revision_of(&raw_bundle(&clean_files))
    );
}

#[test]
fn new_disk_loads_and_v2_bundles_still_enforce_strict_skill_rules_and_v1_keeps_core_guards() {
    let id = ProfileId("personal".into());
    let limits = Limits::default();
    let strict_rejects: Vec<(&str, Files, &str)> = vec![
        (
            "unclosed",
            minimal_profile(&format!("---\n{BODY}")),
            "front-matter-unclosed",
        ),
        ("bare", minimal_profile("---"), "front-matter-unclosed"),
        (
            "unknown",
            minimal_profile(&fm("retired-example: text")),
            "schema",
        ),
        (
            "authority",
            minimal_profile(&fm("allowed-tools: noop")),
            "skill-allowed-tools",
        ),
        (
            "wrongname",
            minimal_profile(&fm("name: elsewhere")),
            "skill-name-mismatch",
        ),
        (
            "big",
            minimal_profile(&"x".repeat(32_769)),
            "skill-too-large",
        ),
        ("agg", agg_profile(), "skill-too-large"),
    ];
    for (name, files, code) in strict_rejects {
        // 1) 새 폴더 로드는 반드시 거절
        let tmp = Temp::new(&format!("strict-disk-{name}"));
        write_profile(tmp.path(), "personal", &files);
        let report = load(tmp.path(), &HostCatalog::demo(), &limits)
            .expect_err(&format!("{name}: 새 폴더는 거절돼야 한다"));
        assert!(report.has(code), "{name}: 기대 {code}\n{report}");
        // 2) BOTPROFILE2 번들 복원도 반드시 거절
        let v2_bundle = raw_v2_bundle(&files);
        let issues = from_bundle(&v2_bundle, &id, &revision_of(&v2_bundle), &limits)
            .expect_err(&format!("{name}: BOTPROFILE2는 거절돼야 한다"));
        assert!(
            issues.iter().any(|i| i.code == code),
            "{name}: 기대 {code}, 실제 {issues:?}"
        );
    }

    // BOTPROFILE1 복원도 BOM·프로필 ID 불일치·예약 도구·해시 불일치는 그대로 거절한다
    let bom_files = minimal_profile(&format!("\u{feff}{BODY}"));
    let bom_v1 = raw_bundle(&bom_files);
    let bom_err = from_bundle(&bom_v1, &id, &revision_of(&bom_v1), &limits).unwrap_err();
    assert!(bom_err.iter().any(|i| i.code == "text-bom"), "{bom_err:?}");

    let good_v1 = raw_bundle(&minimal_profile(VALID_FM));
    let other_id = ProfileId("other".into());
    let mismatch = from_bundle(&good_v1, &other_id, &revision_of(&good_v1), &limits).unwrap_err();
    assert!(
        mismatch.iter().any(|i| i.code == "profile-id-mismatch"),
        "{mismatch:?}"
    );
}
