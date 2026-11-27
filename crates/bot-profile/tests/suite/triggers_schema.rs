//! Invariant tests for `schemas/v1/triggers.schema.json`: schema parity with Rust validation, fixed error codes, and legacy bundle restore.

use crate::support::{Files, append, base, check, edit, ok};
use bot_core::{
    cron::Cron,
    grant::{MAX_PAIRS, MAX_PER_DAY, MAX_PIECE_BYTES, piece_ok},
};
use bot_profile::Schema;
use serde_json::{Value, json};

const TRIGGERS: &str = "agents/main/triggers.yaml";
const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";
const PROFILE: &str = "profile.yaml";

fn triggers_schema() -> Schema {
    let doc: Value =
        serde_json::from_str(include_str!("../../../../schemas/v1/triggers.schema.json"))
            .expect("triggers.schema.json");
    Schema::compile_bundled(doc).expect("compile bundled triggers schema")
}

fn with_notify_and_slack() -> Files {
    let files = append(
        base(),
        TOOLS,
        "  - id: notify\n    class: irreversible\n    effects: true\n    scopes: [calendar.read]\n    input: {type: object, required: [report], properties: {report: {type: string}}, additionalProperties: false}\n",
    );
    edit(
        files,
        PROFILE,
        "  - {identity: \"service:morning\", roles: [reader]}\n",
        "  - {identity: \"service:morning\", roles: [reader]}\n  - {identity: \"slack:home/U1\", roles: [owner]}\n",
    )
}

fn replace_triggers(files: Files, yaml: &str) -> Files {
    files
        .into_iter()
        .map(|(path, content)| {
            if path == TRIGGERS {
                (path, yaml.to_owned())
            } else {
                (path, content)
            }
        })
        .collect()
}

fn trigger_doc(trigger: Value) -> Value {
    json!({"version": 1, "triggers": [trigger]})
}

#[test]
fn preapproved_target_pattern_matches_piece_ok_exactly() {
    let schema = triggers_schema();
    let check_piece = |s: &str| {
        let doc = trigger_doc(json!({
            "id": "notify",
            "on": {"cron": "0 9 * * *"},
            "principal": "service:morning",
            "target": {"tool": "agenda.notify", "args": {"report": "daily"}},
            "preapproved": {
                "max_per_day": 1,
                "targets": [{"operation": s, "target": s}]
            }
        }));
        schema.validate(&doc).is_empty()
    };
    let max_ok = "a".repeat(MAX_PIECE_BYTES);
    let too_long = "a".repeat(MAX_PIECE_BYTES + 1);
    for sample in [
        "post",
        "channel:reports",
        "mail-bot",
        "user@team",
        "a.b_c+1#2",
        ".",
        ".a",
        "a.",
        ".a.",
        "a.b.c",
        max_ok.as_str(),
        "",
        "..",
        "..a",
        "a..",
        "a..b",
        "a.b..c",
        "*",
        "channel:*",
        "https://hooks.example.com/x",
        "a/b",
        "a b",
        "a\tb",
        "line\nbreak",
        "?x",
        "café",
        too_long.as_str(),
    ] {
        assert_eq!(
            check_piece(sample),
            piece_ok(sample),
            "piece_ok and schema pattern disagree: {sample:?}"
        );
    }
}

#[test]
fn cron_pattern_accepts_all_valid_expressions_and_rejects_wrong_field_counts_and_aliases() {
    let schema = triggers_schema();
    let validate_cron = |expr: &str| {
        let doc = trigger_doc(json!({
            "id": "m",
            "on": {"cron": expr},
            "principal": "service:morning",
            "target": {"tool": "agenda.list", "args": {}}
        }));
        schema.validate(&doc)
    };
    for good in [
        "* * * * *",
        "0 9 * * *",
        "*/15 9-17 * JAN-MAR MON-FRI",
        "  0   9 * * *  ",
        "0\t9\t*\t*\t*",
        "0 9 * * *\n",
        "  @daily\r\n",
        "0 9\n* * *",
        "0\u{00a0}9 * * *",
        "0\u{202f}9 * * *",
        "0\u{2003}9 * * *",
        "0\u{3000}9 * * *",
        "\u{2002}@midnight\u{3000}",
        "0 9 ? * MON",
        "0 9 1 * ?",
        "@hourly",
        "@daily",
        "@midnight",
        "@weekly",
        "@monthly",
        "@yearly",
        "@annually",
        " @daily ",
    ] {
        assert!(Cron::parse(good).is_ok(), "{good:?}");
        assert!(
            validate_cron(good).is_empty(),
            "schema rejected valid cron {good:?}: {:?}",
            validate_cron(good)
        );
    }
    for bad in [
        "",
        "   ",
        "\n\r\n",
        "*",
        "* * * *",
        "* * * * * *",
        "0 0 9 * * * *",
        "@reboot",
        "@daily extra",
        "@daily 1 2 3 4",
        "not a cron",
    ] {
        assert!(Cron::parse(bad).is_err(), "{bad:?}");
        assert!(!validate_cron(bad).is_empty(), "schema must reject {bad:?}");
        let yaml = format!(
            "version: 1\ntriggers:\n  - id: m\n    \"on\": {{cron: {}}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.list, args: {{}}}}\n",
            serde_json::to_string(bad).unwrap()
        );
        let report = check(&replace_triggers(base(), &yaml)).expect_err("expected failure");
        assert!(report.has("bad-cron"), "{bad:?}: {report}");
        let msg = report.to_string();
        assert!(
            !msg.contains("(?:") && !msg.contains("^["),
            "{bad:?}: {msg}"
        );
    }
    // 5-field expressions with out-of-range values pass the schema so Rust emits per-field diagnostics.
    for range_bad in ["61 * * * *", "* 24 * * *", "*/0 * * * *", "5-1 * * * *"] {
        assert!(
            validate_cron(range_bad).is_empty(),
            "5-field range errors must reach Rust parser: {range_bad:?}"
        );
        let yaml = format!(
            "version: 1\ntriggers:\n  - id: m\n    \"on\": {{cron: \"{range_bad}\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.list, args: {{}}}}\n"
        );
        let report = check(&replace_triggers(base(), &yaml)).expect_err("expected failure");
        assert!(report.has("bad-cron"), "{range_bad}: {report}");
        assert!(
            report.to_string().contains("field"),
            "{range_bad}: {report}"
        );
    }
}

#[test]
fn boundary_valid_preapproved_and_human_approval_pass_both_schema_and_loader() {
    let schema = triggers_schema();
    let targets: Vec<Value> = (0..MAX_PAIRS)
        .map(|i| json!({"operation": "post", "target": format!("channel:r{i}")}))
        .collect();
    let pre_trigger = json!({
        "id": "notify",
        "on": {"cron": "0 9 * * *"},
        "principal": "service:morning",
        "target": {"tool": "agenda.notify", "args": {"report": "daily"}},
        "preapproved": {"max_per_day": MAX_PER_DAY, "targets": targets}
    });
    assert!(
        schema
            .validate(&trigger_doc(pre_trigger.clone()))
            .is_empty()
    );
    let yaml = format!(
        "version: 1\ntriggers:\n  - {}\n",
        serde_json::to_string(&pre_trigger).unwrap()
    );
    ok(&replace_triggers(with_notify_and_slack(), &yaml));

    let mut members = String::new();
    let mut approvers = Vec::new();
    for i in 1..=16 {
        let id = format!("slack:home/U{i}");
        members.push_str(&format!("  - {{identity: \"{id}\", roles: [owner]}}\n"));
        approvers.push(Value::String(id));
    }
    let human_trigger = json!({
        "id": "confirm",
        "on": {"event": "message", "binding": "team"},
        "match": {"body.text": "approve"},
        "input": {"report": "body.text"},
        "target": {"tool": "agenda.notify"},
        "human_approval": {"approvers": approvers}
    });
    assert!(
        schema
            .validate(&trigger_doc(human_trigger.clone()))
            .is_empty()
    );
    let files = edit(
        with_notify_and_slack(),
        PROFILE,
        "  - {identity: \"slack:home/U1\", roles: [owner]}\n",
        &members,
    );
    let yaml = format!(
        "version: 1\ntriggers:\n  - {}\n",
        serde_json::to_string(&human_trigger).unwrap()
    );
    ok(&replace_triggers(files, &yaml));
}

#[test]
fn schema_rejections_stay_a_subset_of_loader_rejections_and_keep_fixed_codes() {
    let schema = triggers_schema();
    let cron_with = |pre: Value| {
        json!({
            "id": "notify",
            "on": {"cron": "0 9 * * *"},
            "principal": "service:morning",
            "target": {"tool": "agenda.notify", "args": {"report": "daily"}},
            "preapproved": pre
        })
    };
    let evt_with = |human: Value| {
        json!({
            "id": "confirm",
            "on": {"event": "message", "binding": "team"},
            "match": {"body.text": "approve"},
            "input": {"report": "body.text"},
            "target": {"tool": "agenda.notify"},
            "human_approval": human
        })
    };
    let too_many_targets: Vec<Value> = (0..=MAX_PAIRS)
        .map(|i| json!({"operation": "post", "target": format!("c{i}")}))
        .collect();
    let too_many_approvers: Vec<Value> = (1..=17)
        .map(|i| Value::String(format!("slack:home/U{i}")))
        .collect();

    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "empty preapproved object",
            cron_with(json!({})),
            "invalid_preapproval",
        ),
        (
            "unknown key only in preapproved",
            cron_with(json!({"bogus": 1})),
            "invalid_preapproval",
        ),
        (
            "extra key in preapproved",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "a"}], "note": "x"}),
            ),
            "invalid_preapproval",
        ),
        (
            "max_per_day 0",
            cron_with(json!({"max_per_day": 0, "targets": [{"operation": "post", "target": "a"}]})),
            "invalid_preapproval",
        ),
        (
            "max_per_day 65536",
            cron_with(
                json!({"max_per_day": 65536, "targets": [{"operation": "post", "target": "a"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "string max_per_day",
            cron_with(
                json!({"max_per_day": "3", "targets": [{"operation": "post", "target": "a"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "empty targets array",
            cron_with(json!({"max_per_day": 3, "targets": []})),
            "invalid_preapproval",
        ),
        (
            "targets over 64 limit",
            cron_with(json!({"max_per_day": 3, "targets": too_many_targets})),
            "invalid_preapproval",
        ),
        (
            "duplicate target pair across key order",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "a"}, {"target": "a", "operation": "post"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "wildcard target",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "channel:*"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "URL target",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "https://hooks.example.com/x"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "double-dot target",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "a..b"}]}),
            ),
            "invalid_preapproval",
        ),
        (
            "extra key in target pair",
            cron_with(
                json!({"max_per_day": 3, "targets": [{"operation": "post", "target": "a", "params": "x"}]}),
            ),
            "invalid_preapproval",
        ),
        ("non-object preapproved", cron_with(json!(true)), "schema"),
        (
            "empty human_approval object",
            evt_with(json!({})),
            "invalid_human_approval",
        ),
        (
            "unknown key in human_approval",
            evt_with(json!({"bogus": 1})),
            "invalid_human_approval",
        ),
        (
            "empty approvers",
            evt_with(json!({"approvers": []})),
            "invalid_human_approval",
        ),
        (
            "approvers over 16 limit",
            evt_with(json!({"approvers": too_many_approvers})),
            "invalid_human_approval",
        ),
        (
            "duplicate approver",
            evt_with(json!({"approvers": ["slack:home/U1", "slack:home/U1"]})),
            "invalid_human_approval",
        ),
        (
            "service approver",
            evt_with(json!({"approvers": ["service:morning"]})),
            "invalid_human_approval",
        ),
        (
            "local approver",
            evt_with(json!({"approvers": ["local:owner"]})),
            "invalid_human_approval",
        ),
        (
            "lowercase Slack ID",
            evt_with(json!({"approvers": ["slack:home/u1"]})),
            "invalid_human_approval",
        ),
        ("non-object human_approval", evt_with(json!(true)), "schema"),
        (
            "non-string cron",
            json!({"id": "m", "on": {"cron": 123}, "principal": "service:morning", "target": {"tool": "agenda.list", "args": {}}}),
            "schema",
        ),
    ];

    for (name, trigger, code) in cases {
        let doc = trigger_doc(trigger.clone());
        let schema_errs = schema.validate(&doc);
        assert!(
            !schema_errs.is_empty(),
            "{name}: schema accepted an invalid trigger"
        );
        let yaml = format!(
            "version: 1\ntriggers:\n  - {}\n",
            serde_json::to_string(&trigger).unwrap()
        );
        let report =
            check(&replace_triggers(with_notify_and_slack(), &yaml)).expect_err("expected failure");
        assert!(report.has(code), "{name}: expected {code}\n{report}");
        let text = report.to_string();
        assert!(
            !text.contains("(?:") && !text.contains("^[") && !text.contains("(pattern "),
            "{name}: raw regex leaked: {text}"
        );
    }
}

#[test]
fn legacy_v1_bundle_with_multiline_cron_preapproved_and_human_approval_restores_cleanly() {
    use bot_core::{
        id::{ProfileId, Revision},
        port::Reply,
        profile::{ProfileBundles, ProfileCatalog},
    };
    use bot_profile::{Limits, Registry, read_legacy_bundle};
    use std::collections::{BTreeMap, BTreeSet};

    struct MemBundle(ProfileId, Revision, Vec<u8>);
    impl ProfileBundles for MemBundle {
        fn put_bundle(&self, _: ProfileId, _: Revision, _: Vec<u8>) -> Reply<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn bundle(&self, profile: &ProfileId, revision: &Revision) -> Reply<'_, Option<Vec<u8>>> {
            let hit = (*profile == self.0 && *revision == self.1).then(|| self.2.clone());
            Box::pin(async move { Ok(hit) })
        }
        fn created_ms(&self, _: &ProfileId, _: &Revision) -> Reply<'_, Option<i64>> {
            Box::pin(async { Ok(None) })
        }
        fn pinned_revisions<'a>(
            &'a self,
            _: &'a BTreeMap<ProfileId, Revision>,
        ) -> Reply<'a, BTreeSet<(ProfileId, Revision)>> {
            Box::pin(async { Ok(BTreeSet::new()) })
        }
    }

    // Legacy BOTPROFILE1 bundle with YAML block-scalar cron (trailing newline), preapproved, and human_approval.
    let yaml = "\
version: 1
triggers:
  - id: notify
    \"on\":
      cron: |
        0 9 * * *
    principal: \"service:morning\"
    target: {tool: agenda.notify, args: {report: daily}}
    preapproved:
      max_per_day: 3
      targets:
        - {operation: post, target: \"channel:reports\"}
  - id: confirm
    \"on\": {event: message, binding: team}
    match: {body.text: \"approve\"}
    input: {report: body.text}
    target: {tool: agenda.notify}
    human_approval:
      approvers: [\"slack:home/U1\"]
";
    let tmp = crate::support::Temp::new("trig-v1");
    let files = replace_triggers(with_notify_and_slack(), yaml);
    crate::support::write_profile(tmp.path(), "personal", &files);
    let (id, rev, bytes) =
        read_legacy_bundle(&tmp.path().join("personal"), &Limits::default()).unwrap();
    assert!(bytes.starts_with(b"BOTPROFILE1\n"));

    let store = MemBundle(id.clone(), rev.clone(), bytes);
    let registry = Registry::new();
    let wanted = BTreeSet::from([(id.clone(), rev.clone())]);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let restored = rt
        .block_on(registry.restore(&store, &wanted, &Limits::default()))
        .expect("legacy BOTPROFILE1 trigger bundle restore");
    assert_eq!(restored.len(), 1);
    let snap = registry.lookup(&id, &rev).unwrap();
    assert_eq!(
        snap.agents[&bot_core::id::AgentId("main".into())]
            .triggers
            .len(),
        2
    );
}

#[test]
fn cron_schema_whitespace_matches_rust_char_is_whitespace_for_every_unicode_scalar() {
    let doc: Value =
        serde_json::from_str(include_str!("../../../../schemas/v1/triggers.schema.json"))
            .expect("triggers.schema.json");
    let pattern = doc
        .pointer("/$defs/source/oneOf/0/properties/cron/pattern")
        .and_then(Value::as_str)
        .expect("cron pattern");
    let re = regex::Regex::new(pattern).expect("regex compile");
    let schema = triggers_schema();
    let mut ws_count = 0usize;
    for ch in '\0'..=char::MAX {
        let is_ws = ch.is_whitespace();
        let five = format!("0{ch}9 * * *");
        let alias = format!("{ch}@midnight{ch}");
        assert_eq!(
            re.is_match(&five),
            is_ws,
            "cron separator whitespace mismatch: U+{:04X}",
            ch as u32
        );
        assert_eq!(
            re.is_match(&alias),
            is_ws,
            "cron alias surrounding whitespace mismatch: U+{:04X}",
            ch as u32
        );
        if is_ws {
            ws_count += 1;
            assert!(Cron::parse(&five).is_ok(), "U+{:04X}", ch as u32);
            assert!(Cron::parse(&alias).is_ok(), "U+{:04X}", ch as u32);
            for expr in [&five, &alias] {
                let d = trigger_doc(json!({
                    "id": "m",
                    "on": {"cron": expr},
                    "principal": "service:morning",
                    "target": {"tool": "agenda.list", "args": {}}
                }));
                assert!(
                    schema.validate(&d).is_empty(),
                    "schema rejected Rust whitespace U+{:04X}",
                    ch as u32
                );
            }
        }
    }
    assert_eq!(ws_count, 25, "all 25 Unicode White_Space scalars checked");
}
