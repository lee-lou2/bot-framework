//! OpenAI 호환 모델 바인딩(`adapter: http` + `model`): 스키마의 두 HTTP 분기, 한 입구의 여러 모델, 승인자 매핑 검증, 입구 소유,
//! 호스트 종류 대조, 정책 판본.

use crate::support::openai::{
    LOCAL, SLACK, codes, host_openai, load_with, models, ok, with_people,
};
use crate::support::{Files, Temp, append, base, edit, host, write_profile};
use bot_core::{
    id::{BindingId, ProfileId},
    profile::Adapter,
};
use bot_profile::Limits;
use bot_profile::load;

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
    // 모델이 생기면 정책 판본이 달라진다(웹훅 전용 프로필은 따로 골든 시험이 지킨다)
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
        "run 스코프가 있는 멤버와 에이전트"
    );
    // 호출 멤버의 역할에 run이 없다
    let no_run = edit(
        models(""),
        "profile.yaml",
        "{identity: \"http:openai/build\", roles: [owner]}",
        "{identity: \"http:openai/build\", roles: [reader]}",
    );
    assert_eq!(warned(&no_run).len(), 2, "모델 둘 다");
    // 에이전트 상한에 run이 없다: research 모델만 걸린다
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
    assert_eq!(codes(&plain("")), Vec::<&str>::new(), "웹훅 분기");
    assert_eq!(
        codes(&model(", model: gpt-4o")),
        Vec::<&str>::new(),
        "모델 분기(호스트가 openai를 안다)"
    );
    for (name, files) in [
        ("웹훅에 승인자", plain(", approvers: {}")),
        ("모델 없는 OpenAI 승인자", model(", approvers: {}")),
        ("점이 든 모델 이름", model(", model: gpt-4.1")),
        ("대문자 모델 이름", model(", model: GPT")),
        (
            "너무 긴 모델 이름",
            model(&format!(", model: m{}", "a".repeat(63))),
        ),
        ("모델이 비어 있음", model(", model: \"\"")),
        ("알 수 없는 필드", model(", model: a, temperature: 1")),
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
            "로컬에 binding",
            "{\"http:openai/build\": {principal: \"local:owner\", binding: team}}",
        ),
        (
            "Slack에 binding 없음",
            "{\"http:openai/build\": {principal: \"slack:home/U1\"}}",
        ),
        (
            "서비스 주체",
            "{\"http:openai/build\": {principal: \"service:morning\"}}",
        ),
        (
            "HTTP 주체",
            "{\"http:openai/build\": {principal: \"http:openai/ops\"}}",
        ),
        ("키에 입구 없음", "{build: {principal: \"local:owner\"}}"),
        (
            "키에 주체 없음",
            "{\"http:openai\": {principal: \"local:owner\"}}",
        ),
        (
            "알 수 없는 필드",
            "{\"http:openai/build\": {principal: \"local:owner\", note: x}}",
        ),
        (
            "소문자 Slack ID",
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
            "요청자가 멤버가 아님",
            "{\"http:openai/ops\": {principal: \"local:owner\"}}",
            "approver-requester-unknown",
        ),
        (
            "다른 입구의 주체",
            "{\"http:reminders/ci\": {principal: \"local:owner\"}}",
            "approver-requester-endpoint",
        ),
        (
            "승인자가 멤버가 아님",
            "{\"http:openai/build\": {principal: \"local:ghost\"}}",
            "approver-unknown",
        ),
        (
            "전달 바인딩이 없음",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: nowhere}}",
            "approver-binding-unknown",
        ),
        (
            "전달 바인딩이 CLI",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: desk}}",
            "approver-binding-kind",
        ),
        (
            "전달 바인딩이 DM",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: direct}}",
            "approver-binding-kind",
        ),
        (
            "전달 바인딩이 모델 바인딩",
            "{\"http:openai/build\": {principal: \"slack:home/U1\", binding: research}}",
            "approver-binding-kind",
        ),
        (
            "다른 워크스페이스",
            "{\"http:openai/build\": {principal: \"slack:other/U2\", binding: team}}",
            "approver-workspace-mismatch",
        ),
    ];
    for (name, approvers, code) in cases {
        let found = codes(&with_people(models(approvers)));
        assert!(found.contains(&code), "{name}: 기대 {code}, 실제 {found:?}");
    }
}

#[test]
fn no_trigger_can_target_a_model_binding() {
    let http = "  - id: hook\n    \"on\": {http: {method: POST, path: /x}, binding: assistant}\n    target: {instruction: x}\n";
    let message = "  - id: chat\n    \"on\": {event: message, binding: assistant}\n    target: {instruction: x}\n";
    for (name, extra) in [("HTTP 트리거", http), ("메시지 트리거", message)] {
        let files = append(models(""), TRIGGERS, extra);
        assert!(
            codes(&files).contains(&"openai-binding-no-trigger"),
            "{name}: {:?}",
            codes(&files)
        );
    }
    // 웹훅 바인딩의 트리거는 그대로 통한다
    let files = append(
        models(""),
        TRIGGERS,
        "  - id: hook\n    \"on\": {http: {method: POST, path: /x}, binding: inbox}\n    target: {instruction: x}\n",
    );
    assert_eq!(codes(&files), Vec::<&str>::new());
}

#[test]
fn an_endpoint_has_one_owner_and_one_kind() {
    // 같은 프로필의 같은 모델 이름
    let twice = edit(
        models(""),
        "profile.yaml",
        "model: research",
        "model: assistant",
    );
    assert_eq!(codes(&twice), ["duplicate-entrance"]);
    // 같은 별칭에 웹훅 바인딩과 모델 바인딩이 섞임
    let mixed = edit(
        models(""),
        "profile.yaml",
        "endpoint: reminders",
        "endpoint: openai",
    );
    assert_eq!(codes(&mixed), ["endpoint-kind-mixed"]);
    // 다른 프로필이 같은 OpenAI 별칭을 쓰면 모델 이름이 달라도 모호하다
    let root = Temp::new("owners");
    write_profile(root.path(), "personal", &models(""));
    let second = vec![
        (
            "profile.yaml".to_owned(),
            "version: 1\nid: second\nmodel: assistant\nroles: {r: [run]}\nmembers: []\nbindings:\n  - {id: other, adapter: http, endpoint: openai, model: other, agent: main}\npolicy: {scopes: [run]}\n".to_owned(),
        ),
        ("agents/main/AGENT.md".to_owned(), "둘째.\n".to_owned()),
    ];
    write_profile(root.path(), "second", &second);
    let report = load(root.path(), &host_openai(), &Limits::default()).unwrap_err();
    assert_eq!(report.codes(), ["duplicate-entrance"]);
    let issue = report.errors().next().unwrap();
    assert!(issue.path.starts_with("second/"), "{}", issue.path);
    assert!(issue.message.contains("소유"), "{}", issue.message);
}

#[test]
fn a_mapping_for_a_subject_without_a_key_slot_only_warns() {
    // 승인 매핑은 프로필의 지정이고 키 슬롯은 호스트의 자격 발급이다: 슬롯이 없는 주체의 매핑은 활성화를 막지 않고 경고만 한다
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
    // 슬롯이 있는 주체의 매핑은 경고도 없다
    let ok_set = load_with(&models(LOCAL), &host_openai()).unwrap();
    assert!(
        ok_set
            .warnings()
            .iter()
            .all(|w| w.code != "approver-key-unknown")
    );
    // 요청자·승인자 멤버 검증은 그대로 오류다
    let no_member = models("{\"http:openai/ghost\": {principal: \"local:owner\"}}");
    assert!(codes(&no_member).contains(&"approver-requester-unknown"));
}

#[test]
fn the_host_must_know_the_endpoint_with_the_matching_kind() {
    let cases: [(&str, Files, &str); 4] = [
        (
            "모델 바인딩을 웹훅 별칭에",
            edit(
                models(""),
                "profile.yaml",
                "endpoint: openai, model: research",
                "endpoint: alerts, model: research",
            ),
            "endpoint-kind-mismatch",
        ),
        (
            "웹훅 바인딩을 OpenAI 별칭에",
            edit(
                base(),
                "profile.yaml",
                "endpoint: reminders",
                "endpoint: openai",
            ),
            "endpoint-kind-mismatch",
        ),
        (
            "등록하지 않은 별칭",
            edit(
                models(""),
                "profile.yaml",
                "endpoint: openai, model: research",
                "endpoint: nowhere, model: research",
            ),
            "unknown-endpoint",
        ),
        (
            "OpenAI와 함께 쓰는 웹훅 v1",
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
        assert!(report.has(code), "{name}: 기대 {code}\n{report}");
    }
    // 웹훅 `v1` 단독은 옛 그대로 통한다
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
    // 모델이 다른 에이전트를 가리키면 달라진다
    let agent = edit(
        with_people(models("")),
        "profile.yaml",
        "model: assistant, agent: main}",
        "model: assistant, agent: main/research}",
    );
    assert_ne!(rev(&agent), without);
    // 바인딩을 적는 순서는 판본 바이트(revision)만 바꾸고 정책 판본은 그대로다
    let (a, b) = (ok(&models("")), ok(&swapped(&models(""))));
    assert_eq!(a.policy_revision, b.policy_revision);
    assert_ne!(a.revision, b.revision);
}

/// assistant와 research 바인딩 줄의 순서만 바꾼다.
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
