//! Tests for OpenAI-compatible model bindings (`adapter: http` + `model`): schema branches, approvers, endpoint ownership, host checks, and policy revision.

use crate::support::openai::{
    LOCAL, SLACK, codes, host_openai, load_with, models, ok, with_people,
};
use crate::support::{Files, Temp, append, base, edit, host, write_profile};
use bot_core::{
    id::{BindingId, ProfileId},
    profile::Adapter,
};
use bot_profile::{Limits, load};

const TRIGGERS: &str = "agents/main/triggers.yaml";

#[test]
fn models_share_one_endpoint_and_leave_webhooks_alone() {
    let snap = ok(&models(""));
    let source = |id: &str| &snap.bindings[&BindingId(id.into())].source;
    assert!(matches!(
        source("assistant"),
        Adapter::OpenAi { endpoint, model, approvers }
            if endpoint == "openai" && model == "assistant" && approvers.is_empty()
    ));
    assert!(matches!(source("research"), Adapter::OpenAi { model, .. } if model == "research"));
    assert_eq!(
        snap.bindings[&BindingId("research".into())].agent.0,
        "main/research"
    );
    assert!(matches!(source("inbox"), Adapter::Http { endpoint } if endpoint == "reminders"));
    let set = load_with(&models(""), &host_openai()).unwrap();
    assert_eq!(set.warnings().len(), 0, "{:?}", set.warnings());
    let plain = load_with(&base(), &host_openai()).unwrap();
    let plain = plain.profile(&ProfileId("personal".into())).unwrap();
    assert_ne!(plain.policy_revision, snap.policy_revision);
}

#[test]
fn a_model_without_a_matching_member_warns_that_nobody_can_use_it() {
    let files = edit(
        models(""),
        "profile.yaml",
        "  - {identity: \"http:openai/build\", roles: [owner]}\n",
        "",
    );
    let set = load_with(&files, &host_openai()).unwrap();
    assert!(set.warnings().iter().any(|w| w.code == "binding-no-member"));
}

#[test]
fn a_model_nobody_can_run_warns() {
    let warned = |files: &Files| -> Vec<String> {
        let set = load_with(files, &host_openai()).unwrap_or_else(|r| panic!("{r}"));
        set.warnings()
            .iter()
            .filter(|w| w.code == "openai-no-runner")
            .map(|w| w.message.clone())
            .collect()
    };
    assert!(
        warned(&models("")).is_empty(),
        "member and agent both have run scope"
    );
    let no_run = edit(
        models(""),
        "profile.yaml",
        "{identity: \"http:openai/build\", roles: [owner]}",
        "{identity: \"http:openai/build\", roles: [reader]}",
    );
    assert_eq!(warned(&no_run).len(), 2, "both models lack a runner");
    let capped = edit(
        models(""),
        "profile.yaml",
        "main/research: [run, calendar.read]",
        "main/research: [calendar.read]",
    );
    let found = warned(&capped);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("research"), "{found:?}");
}

#[test]
fn the_schema_has_two_disjoint_http_branches() {
    let plain = |tail: &str| {
        edit(
            base(),
            "profile.yaml",
            "  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
            &format!("  - {{id: inbox, adapter: http, endpoint: reminders, agent: main{tail}}}"),
        )
    };
    let model = |tail: &str| {
        edit(
            base(),
            "profile.yaml",
            "  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
            &format!("  - {{id: inbox, adapter: http, endpoint: openai, agent: main{tail}}}"),
        )
    };
    assert_eq!(codes(&plain("")), Vec::<&str>::new(), "webhook branch");
    assert_eq!(
        codes(&model(", model: gpt-4o")),
        Vec::<&str>::new(),
        "model branch"
    );
    for (name, files) in [
        ("approvers on webhook", plain(", approvers: {}")),
        ("approvers without model", model(", approvers: {}")),
        ("dot in model name", model(", model: gpt-4.1")),
        ("uppercase model name", model(", model: GPT")),
        (
            "overlong model name",
            model(&format!(", model: m{}", "a".repeat(63))),
        ),
        ("empty model", model(", model: \"\"")),
        ("unknown field", model(", model: a, temperature: 1")),
    ] {
        assert!(codes(&files).contains(&"schema"), "{name}");
    }
    let longest = format!("m{}", "a".repeat(62));
    assert!(!codes(&model(&format!(", model: {longest}"))).contains(&"schema"));
}

#[test]
fn approver_shapes_are_checked_by_the_schema() {
    let cases = [
        (
            "binding on local principal",
            "{\"http:openai/build\": {principal: \"local:owner\", binding: team}}",
        ),
        (
            "Slack principal without binding",
            "{\"http:openai/build\": {principal: \"slack:home/U1\"}}",
        ),
        (
            "service principal",
            "{\"http:openai/build\": {principal: \"service:morning\"}}",
        ),
        (
            "HTTP principal",
            "{\"http:openai/build\": {principal: \"http:openai/ops\"}}",
        ),
        (
            "key without endpoint",
            "{build: {principal: \"local:owner\"}}",
        ),
        (
            "key without subject",
            "{\"http:openai\": {principal: \"local:owner\"}}",
        ),
        (
            "unknown field",
            "{\"http:openai/build\": {principal: \"local:owner\", note: x}}",
        ),
        (
            "lowercase Slack ID",
            "{\"http:openai/build\": {principal: \"slack:home/u1\", binding: team}}",
        ),
    ];
    for (name, approvers) in cases {
        assert!(
            codes(&with_people(models(approvers))).contains(&"schema"),
            "{name}"
        );
    }
}

#[test]
fn approver_mappings_must_point_at_members_and_the_same_installation() {
    let cases: [(&str, &str, &str); 8] = [
        (
            "requester not a member",
            "{\"http:openai/ops\": {principal: \"local:owner\"}}",
            "approver-requester-unknown",
        ),
        (
            "requester on another endpoint",
            "{\"http:reminders/ci\": {principal: \"local:owner\"}}",
            "approver-requester-endpoint",
        ),
        (
            "approver not a member",
            "{\"http:openai/build\": {principal: \"local:ghost\"}}",
            "approver-unknown",
        ),
        (
            "missing delivery binding",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: nowhere}}",
            "approver-binding-unknown",
        ),
        (
            "delivery binding is CLI",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: desk}}",
            "approver-binding-kind",
        ),
        (
            "delivery binding is DM",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: direct}}",
            "approver-binding-kind",
        ),
        (
            "delivery binding is model",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: research}}",
            "approver-binding-kind",
        ),
        (
            "different workspace",
            "{\"http:openai/build\": {principal: \"slack:other/U2\", binding: team}}",
            "approver-workspace-mismatch",
        ),
    ];
    for (name, approvers, code) in cases {
        let found = codes(&with_people(models(approvers)));
        assert!(
            found.contains(&code),
            "{name}: expected {code}, got {found:?}"
        );
    }
}

#[test]
fn no_trigger_can_target_a_model_binding() {
    let http = "  - id: hook\n    \"on\": {http: {method: POST, path: /x}, binding: assistant}\n    target: {instruction: x}\n";
    let message = "  - id: chat\n    \"on\": {event: message, binding: assistant}\n    target: {instruction: x}\n";
    for (name, extra) in [("HTTP trigger", http), ("message trigger", message)] {
        let files = append(models(""), TRIGGERS, extra);
        assert!(
            codes(&files).contains(&"openai-binding-no-trigger"),
            "{name}: {:?}",
            codes(&files)
        );
    }
    let files = append(
        models(""),
        TRIGGERS,
        "  - id: hook\n    \"on\": {http: {method: POST, path: /x}, binding: inbox}\n    target: {instruction: x}\n",
    );
    assert_eq!(codes(&files), Vec::<&str>::new());
}

#[test]
fn an_endpoint_has_one_owner_and_one_kind() {
    let twice = edit(
        models(""),
        "profile.yaml",
        "model: research",
        "model: assistant",
    );
    assert_eq!(codes(&twice), ["duplicate-entrance"]);
    let mixed = edit(
        models(""),
        "profile.yaml",
        "endpoint: reminders",
        "endpoint: openai",
    );
    assert_eq!(codes(&mixed), ["endpoint-kind-mixed"]);
    // Two active profiles cannot share an OpenAI endpoint alias even with distinct model names.
    let root = Temp::new("owners");
    write_profile(root.path(), "personal", &models(""));
    let second = vec![
        (
            "profile.yaml".to_owned(),
            "version: 1\nid: second\nmodel: assistant\nroles: {r: [run]}\nmembers: []\nbindings:\n  - {id: other, adapter: http, endpoint: openai, model: other, agent: main}\npolicy: {scopes: [run]}\n".to_owned(),
        ),
        ("agents/main/AGENT.md".to_owned(), "Second.\n".to_owned()),
    ];
    write_profile(root.path(), "second", &second);
    let report = load(root.path(), &host_openai(), &Limits::default()).unwrap_err();
    assert_eq!(report.codes(), ["duplicate-entrance"]);
    let issue = report.errors().next().unwrap();
    assert!(issue.path.starts_with("second/"), "{}", issue.path);
    let lower = issue.message.to_lowercase();
    assert!(
        lower.contains("own") || lower.contains("single") || lower.contains("one"),
        "{}",
        issue.message
    );
}

#[test]
fn a_mapping_for_a_subject_without_a_key_slot_only_warns() {
    // Missing host key slot warns instead of failing activation so revoking a key never blocks profile reload.
    let files = edit(
        models("{\"http:openai/guest\": {principal: \"local:owner\"}}"),
        "profile.yaml",
        "bindings:\n",
        "  - {identity: \"http:openai/guest\", roles: [owner]}\nbindings:\n",
    );
    let set = load_with(&files, &host_openai()).unwrap_or_else(|r| panic!("{r}"));
    let found: Vec<_> = set
        .warnings()
        .iter()
        .filter(|w| w.code == "approver-key-unknown")
        .collect();
    assert_eq!(found.len(), 1, "{:?}", set.warnings());
    assert!(found[0].message.contains("http:openai/guest"));
    let ok_set = load_with(&models(LOCAL), &host_openai()).unwrap();
    assert!(
        ok_set
            .warnings()
            .iter()
            .all(|w| w.code != "approver-key-unknown")
    );
    let no_member = models("{\"http:openai/ghost\": {principal: \"local:owner\"}}");
    assert!(codes(&no_member).contains(&"approver-requester-unknown"));
}

#[test]
fn the_host_must_know_the_endpoint_with_the_matching_kind() {
    let cases: [(&str, Files, &str); 4] = [
        (
            "model binding on webhook alias",
            edit(
                models(""),
                "profile.yaml",
                "endpoint: openai, model: research",
                "endpoint: alerts, model: research",
            ),
            "endpoint-kind-mismatch",
        ),
        (
            "webhook binding on OpenAI alias",
            edit(
                base(),
                "profile.yaml",
                "endpoint: reminders",
                "endpoint: openai",
            ),
            "endpoint-kind-mismatch",
        ),
        (
            "unregistered alias",
            edit(
                models(""),
                "profile.yaml",
                "endpoint: openai, model: research",
                "endpoint: nowhere, model: research",
            ),
            "unknown-endpoint",
        ),
        (
            "webhook v1 alongside OpenAI",
            edit(
                base(),
                "profile.yaml",
                "endpoint: reminders",
                "endpoint: v1",
            ),
            "webhook-v1-conflict",
        ),
    ];
    let mut with_v1 = host_openai();
    with_v1.http_endpoints.insert("v1".into());
    for (name, files, code) in cases {
        let report = load_with(&files, &with_v1).expect_err(name);
        assert!(report.has(code), "{name}: expected {code}\n{report}");
    }
    let mut alone = host();
    alone.http_endpoints.insert("v1".into());
    let files = edit(
        base(),
        "profile.yaml",
        "endpoint: reminders",
        "endpoint: v1",
    );
    load_with(&files, &alone).unwrap_or_else(|r| panic!("{r}"));
}

#[test]
fn the_policy_revision_follows_the_model_and_approval_mapping() {
    let rev = |files: &Files| ok(files).policy_revision;
    let without = rev(&with_people(models("")));
    let local = rev(&with_people(models(LOCAL)));
    let slack = rev(&with_people(models(SLACK)));
    assert_ne!(without, local);
    assert_ne!(local, slack);
    let agent = edit(
        with_people(models("")),
        "profile.yaml",
        "model: assistant, agent: main}",
        "model: assistant, agent: main/research}",
    );
    assert_ne!(rev(&agent), without);
    // Reordering bindings changes raw bundle revision only, not policy_revision.
    let (a, b) = (ok(&models("")), ok(&swapped(&models(""))));
    assert_eq!(a.policy_revision, b.policy_revision);
    assert_ne!(a.revision, b.revision);
}

fn swapped(files: &Files) -> Files {
    let text = &files.iter().find(|(p, _)| p == "profile.yaml").unwrap().1;
    let lines: Vec<&str> = text.lines().collect();
    let at = |id: &str| {
        lines
            .iter()
            .position(|l| l.contains(&format!("{{id: {id},")))
            .unwrap()
    };
    let mut lines = lines.clone();
    lines.swap(at("assistant"), at("research"));
    edit(
        files.clone(),
        "profile.yaml",
        text,
        &format!("{}\n", lines.join("\n")),
    )
}
