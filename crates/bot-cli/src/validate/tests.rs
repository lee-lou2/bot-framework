//! `bot validate` 단위 시험: 모델 키·민감 트리거 지문 키·모델 VM 점검·OpenAI 키 슬롯 검증.

use super::*;
use crate::host;
use bot_gateway::testing::profile;

fn host_with(providers: &str) -> HostFile {
    host::parse(&format!(
        r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{providers}}}"#
    ))
    .unwrap()
}

const PROVIDERS: &str = r#", "providers": {
    "assistant": {"kind": "claude-cli", "model": "m1", "endpoint": "https://api.example", "secret": "model-key"},
    "plain": {"kind": "demo"},
    "other": {"kind": "claude-cli", "model": "m2", "endpoint": "https://api.example", "secret": "work-key"}}"#;

#[test]
fn a_profile_needs_the_service_keys_of_the_claude_aliases_it_is_allowed_to_use() {
    let snapshot = profile("personal").build();
    let host = host_with(PROVIDERS);
    // 허용한 별칭 중 claude-cli인 것(assistant)의 키만이다: 데모 별칭(plain)과 다른 프로필의 별칭(other: work-key)은 아니다
    assert_eq!(
        model_secrets(&snapshot, &host),
        BTreeSet::from(["model-key".to_owned()])
    );
    // validate와 관측(알려진 비밀 가리기)이 쓰는 목록에 들어간다
    let needed = needed_secrets(&snapshot, &host);
    assert!(needed.contains("model-key"), "{needed:?}");
    assert!(!needed.contains("work-key"), "{needed:?}");
}

#[test]
fn a_profile_with_a_private_trigger_needs_its_fingerprint_key_and_one_without_does_not() {
    use bot_gateway::testing::{event_trigger, tool_target};
    let gateway = HostConfig::from_json(
        r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token"}}}}"#,
    )
    .unwrap();
    let dm = bot_core::profile::Adapter::SlackDm {
        workspace: "home".into(),
    };
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    let private = profile("personal")
        .binding("direct", dm.clone(), "main")
        .trigger("main", secure)
        .build();
    let names = entrance_secrets(&private, &gateway);
    assert!(names.contains("private-intake"), "{names:?}");
    // 입구 자격은 그대로 있고, 민감 트리거가 없으면 지문 키를 요구하지 않는다
    assert!(
        names.contains("bot-token") && names.contains("app-token"),
        "{names:?}"
    );
    let plain = profile("personal").binding("direct", dm, "main").build();
    assert!(!entrance_secrets(&plain, &gateway).contains("private-intake"));
    // 지문 키는 validate·관측이 쓰는 필요한 자격 목록에도 들어간다
    let host =
        host_with(r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#);
    assert!(needed_secrets(&private, &host).contains("private-intake"));
}

#[test]
fn a_claude_alias_needs_the_model_vm_in_the_host_config() {
    let snapshot = profile("personal").build();
    let apple = format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "bot-internal"}}}}"#,
        "a".repeat(64)
    );
    // claude-cli를 허용했는데 VM 설정이 없으면 막는다(srt만 있어도 마찬가지다)
    let without = host_with(PROVIDERS);
    let message = missing_model_sandbox(&snapshot, &without).unwrap();
    assert!(
        message.contains("personal") && message.contains("sandbox.apple"),
        "{message}"
    );
    let srt = host_with(&format!(
        r#"{PROVIDERS}, "sandbox": {{"srt": {{"binary": "/usr/bin/true", "work_root": "/tmp/w"}}}}"#
    ));
    assert!(missing_model_sandbox(&snapshot, &srt).is_some());
    // VM 설정이 있으면 통과, 데모 별칭만 쓰는 호스트는 VM이 필요 없다
    assert!(missing_model_sandbox(&snapshot, &host_with(&format!("{PROVIDERS}{apple}"))).is_none());
    let demo =
        host_with(r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#);
    assert!(missing_model_sandbox(&snapshot, &demo).is_none());
}

#[test]
fn hosts_without_claude_aliases_or_without_the_profile_in_the_model_table_need_no_model_key() {
    let snapshot = profile("personal").build();
    let demo =
        host_with(r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#);
    assert!(model_secrets(&snapshot, &demo).is_empty());
    // 호스트가 이 프로필에 별칭을 허용하지 않았다
    let other = host::parse(&format!(
        r#"{{"version": 1, "models": {{"work": ["other"]}}{PROVIDERS}}}"#
    ))
    .unwrap();
    assert!(model_secrets(&snapshot, &other).is_empty());
    // providers 절이 없는 호스트(게이트웨이만 쓰는 호스트)
    assert!(model_secrets(&snapshot, &host::parse(r#"{"version": 1}"#).unwrap()).is_empty());
}

fn openai_gateway() -> HostConfig {
    HostConfig::from_json(
        r#"{"http": {"endpoints": {"openai": {"kind": "openai", "keys": {
            "build": {"secret": "build-key", "subject": "build"},
            "next": {"secret": "next-key", "subject": "build"}}}}}}"#,
    )
    .unwrap()
}

fn openai_profile(id: &str) -> std::sync::Arc<Snapshot> {
    profile(id)
        .binding(
            "assistant",
            bot_core::profile::Adapter::OpenAi {
                endpoint: "openai".into(),
                model: "assistant".into(),
                approvers: Default::default(),
            },
            "main",
        )
        .build()
}

#[test]
fn an_openai_binding_needs_the_credentials_of_every_key_slot_of_its_entrance() {
    let names = entrance_secrets(&openai_profile("personal"), &openai_gateway());
    assert_eq!(
        names,
        BTreeSet::from(["build-key".to_owned(), "next-key".to_owned()])
    );
    // 입구를 쓰지 않는 프로필은 그 자격을 요구하지 않는다
    assert!(entrance_secrets(&profile("personal").build(), &openai_gateway()).is_empty());
    // validate·관측(알려진 비밀 가리기)이 쓰는 필요한 자격 목록에도 들어간다
    let host = host::parse(
        r#"{"version": 1, "gateway": {"http": {"endpoints": {"openai": {"kind": "openai", "keys": {
            "build": {"secret": "build-key", "subject": "build"}}}}}}}"#,
    )
    .unwrap();
    assert!(needed_secrets(&openai_profile("personal"), &host).contains("build-key"));
    assert!(!needed_secrets(&profile("personal").build(), &host).contains("build-key"));
}

#[tokio::test]
async fn openai_key_slots_must_hold_key_shaped_distinct_values() {
    use bot_gateway::testing::MemorySecrets;
    let good = "good-key-0123456789abcdef0123456789ab";
    let other = "other-key-0123456789abcdef0123456789a";
    let snapshots = [openai_profile("personal")];
    let check = |entries: &[(&str, &str, &[u8])]| {
        let store = MemorySecrets::new(entries);
        let snapshots = snapshots.clone();
        async move { weak_openai_keys(&*store, &snapshots, &openai_gateway()).await }
    };
    // 모양도 맞고 서로 다르면 문제가 없다
    let fine = check(&[
        ("personal", "build-key", good.as_bytes()),
        ("personal", "next-key", other.as_bytes()),
    ])
    .await;
    assert!(fine.is_empty(), "{fine:?}");
    // 값 끝의 줄바꿈은 값이 아니다(인증과 같은 규칙)
    let newline = format!("{good}\n");
    assert!(
        check(&[
            ("personal", "build-key", newline.as_bytes()),
            ("personal", "next-key", other.as_bytes()),
        ])
        .await
        .is_empty()
    );
    // 너무 짧거나·공백이 있거나·ASCII가 아닌 값
    for bad in [
        &b"short"[..],
        b"has a space 0123456789abcdef01234567",
        "한글키-0123456789abcdef0123456789abcd".as_bytes(),
    ] {
        let problems = check(&[
            ("personal", "build-key", bad),
            ("personal", "next-key", other.as_bytes()),
        ])
        .await;
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("build") && problems[0].contains("키 모양"),
            "{problems:?}"
        );
        // 값은 어디에도 싣지 않는다
        assert!(
            !problems[0].contains("short") && !problems[0].contains("한글키"),
            "{problems:?}"
        );
    }
    // 같은 값이 슬롯 둘에 있으면 그 키는 인증되지 않는다 — 시작 전에 막고, 값은 싣지 않는다
    let same = check(&[
        ("personal", "build-key", good.as_bytes()),
        ("personal", "next-key", good.as_bytes()),
    ])
    .await;
    assert_eq!(same.len(), 1, "{same:?}");
    assert!(
        same[0].contains("같은 값") && !same[0].contains(good),
        "{same:?}"
    );
    // 없는 값은 여기서 알리지 않는다(`missing_named`의 몫)
    let missing = check(&[("personal", "build-key", good.as_bytes())]).await;
    assert!(missing.is_empty(), "{missing:?}");
}

#[tokio::test]
async fn the_same_key_value_in_two_profiles_is_caught_too() {
    use bot_gateway::testing::MemorySecrets;
    let value = "dup-key-0123456789abcdef0123456789abc";
    let gateway = HostConfig::from_json(
        r#"{"http": {"endpoints": {
            "alpha": {"kind": "openai", "keys": {"a": {"secret": "a-key", "subject": "build"}}},
            "beta": {"kind": "openai", "keys": {"b": {"secret": "b-key", "subject": "build"}}}}}}"#,
    )
    .unwrap();
    let on = |profile_id: &str, endpoint: &str| {
        profile(profile_id)
            .binding(
                "assistant",
                bot_core::profile::Adapter::OpenAi {
                    endpoint: endpoint.into(),
                    model: "assistant".into(),
                    approvers: Default::default(),
                },
                "main",
            )
            .build()
    };
    let store = MemorySecrets::new(&[
        ("one", "a-key", value.as_bytes()),
        ("two", "b-key", value.as_bytes()),
    ]);
    let problems =
        weak_openai_keys(&*store, &[on("one", "alpha"), on("two", "beta")], &gateway).await;
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(!problems[0].contains(value), "{problems:?}");
}
