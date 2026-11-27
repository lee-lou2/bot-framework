//! Regression tests for `BOTPROFILE1` legacy skill restore compatibility and `BOTPROFILE2` revision separation.

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
    let half = bot_profile::SKILL_MAX_BYTES / 2;
    let mut files = minimal_profile(&"a".repeat(half));
    files.push((
        "agents/main/agents/research/AGENT.md".to_owned(),
        "Child agent.\n".to_owned(),
    ));
    files.push((
        "agents/main/skills/other/SKILL.md".to_owned(),
        "b".repeat(bot_profile::SKILL_MAX_BYTES - half + 1),
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
    let big = "x".repeat(bot_profile::SKILL_MAX_BYTES + 1);
    let crlf = VALID_FM.replace('\n', "\r\n");
    let unclosed = format!("---\n{BODY}");
    let unknown = fm("retired-example: text");
    let authority = fm("allowed-tools: noop");
    let wrongname = fm("name: elsewhere");
    let agg_memo = "a".repeat(bot_profile::SKILL_MAX_BYTES / 2);
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
            .unwrap_or_else(|e| panic!("{name}: legacy v1 restore failed: {e:?}"));
        assert_eq!(restored.revision, v1_rev, "{name}");
        let memo = &restored.agents[&main].skills["memo"];
        assert_eq!(
            memo.instructions.text, expected_memo,
            "{name}: restored instructions must match legacy bytes"
        );
        assert_eq!(
            memo.description, None,
            "{name}: legacy bundles have no skill description"
        );
        assert!(
            memo.tools.is_empty(),
            "{name}: front matter must never grant tools"
        );

        let tmp = Temp::new(&format!("legacy-put-{name}"));
        write_profile(tmp.path(), "personal", &files);
        let (put_id, put_rev, put_bytes) =
            read_legacy_bundle(&tmp.path().join("personal"), &limits)
                .unwrap_or_else(|r| panic!("{name}: read_legacy_bundle failed:\n{r}"));
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
        "profiles with skill front matter must use BOTPROFILE2"
    );
    assert_ne!(
        v2_snap.revision, v1_rev,
        "V1 and V2 interpretations of the same tree must have distinct revisions"
    );
    let memo = &v2_snap.agents[&main].skills["memo"];
    assert_eq!(memo.description.as_deref(), Some("HEADER_SENTINEL"));
    assert_eq!(memo.instructions.text, "Visible stable paragraph.");
    assert!(!memo.instructions.text.contains("HEADER_SENTINEL"));
    let restored_v2 = from_bundle(v2_bundle, &id, &v2_snap.revision, &limits).unwrap();
    assert_eq!(format!("{restored_v2:?}"), format!("{:?}", **v2_snap));
    let (_, read_rev, read_bytes) = read_bundle(&tmp.path().join("personal"), &limits).unwrap();
    assert_eq!(read_rev, v2_snap.revision);
    assert_eq!(read_bytes, v2_bundle);

    // Stripped `name`-only front matter still requires BOTPROFILE2 to distinguish its revision from V1.
    let name_only = minimal_profile(&fm("name: memo"));
    let tmp_name = Temp::new("v2-name-only");
    write_profile(tmp_name.path(), "personal", &name_only);
    let set_name = load(tmp_name.path(), &HostCatalog::demo(), &limits).unwrap();
    assert!(set_name.bundle(&id).unwrap().starts_with(b"BOTPROFILE2\n"));
    assert_ne!(
        set_name.profile(&id).unwrap().revision,
        revision_of(&raw_bundle(&name_only))
    );

    // Plain skills without front matter keep the BOTPROFILE1 format and revision.
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
            minimal_profile(&"x".repeat(bot_profile::SKILL_MAX_BYTES + 1)),
            "skill-too-large",
        ),
        ("agg", agg_profile(), "skill-too-large"),
    ];
    for (name, files, code) in strict_rejects {
        let tmp = Temp::new(&format!("strict-disk-{name}"));
        write_profile(tmp.path(), "personal", &files);
        let report = load(tmp.path(), &HostCatalog::demo(), &limits)
            .expect_err(&format!("{name}: disk load must fail"));
        assert!(report.has(code), "{name}: expected {code}\n{report}");
        let v2_bundle = raw_v2_bundle(&files);
        let issues = from_bundle(&v2_bundle, &id, &revision_of(&v2_bundle), &limits)
            .expect_err(&format!("{name}: BOTPROFILE2 restore must fail"));
        assert!(
            issues.iter().any(|i| i.code == code),
            "{name}: expected {code}, got {issues:?}"
        );
    }

    // Legacy V1 restore still enforces core invariants (BOM, profile ID mismatch, hash, reserved tools).
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
