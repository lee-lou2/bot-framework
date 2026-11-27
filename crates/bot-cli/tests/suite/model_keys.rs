//! 모델 서비스 키(`claude-cli` 별칭의 `secret`) 점검: `bot validate`가 키가 없으면 통과하지 않고, worker는 활성 판본을 게시하기 **전에** 멈춘다.
//! 키는 값이 아니라 이름이 호스트 설정에 있고, 값은 보관소에서 읽는다. 실제 모델·CLI는 쓰지 않는다.

use crate::claude_support::*;
use crate::process_support::*;
use bot_broker::testing::FakeSecrets;
use bot_cli::cli::Role;
use std::sync::Arc;

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run]
members:
  - {identity: "local:owner", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
secrets: []
policy:
  scopes: [run]
  approve: []
"#;

#[test]
fn validate_fails_without_the_model_service_key_and_names_where_to_put_it() {
    let dir = tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let providers = r#", "providers": {"assistant": {"kind": "claude-cli", "model": "model-large",
                "endpoint": "https://api.example", "secret": "model-key"}}"#;
    // 모델 VM 설정(`sandbox.apple`)이 있는 호스트와 없는 호스트
    let vm = format!(
        r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "bot-internal"}}}}"#,
        "a".repeat(64)
    );
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", &format!("{providers}{vm}")),
    )
    .unwrap();
    let bare = dir.path().join("host-bare.json");
    std::fs::write(&bare, host_json(my_uid(), &pkg, "", providers)).unwrap();
    let socket = dir.path().join("g.sock");
    let env_on = |host: &std::path::Path, extra: &[(&str, &str)]| {
        let mut pairs = vec![("BOT_HOST_CONFIG", host.to_str().unwrap())];
        pairs.extend_from_slice(extra);
        env_for(
            &examples(),
            "postgres://unused@localhost/none",
            &socket,
            &pairs,
        )
    };
    let env = |extra: &[(&str, &str)]| env_on(&host, extra);
    // 키가 없다: 통과하지 않고 자격 이름과 보관소 위치(환경 변수)를 알려 준다
    let missing = bot(&refs(&env(&[])), &["validate"]);
    assert_eq!(missing.status.code(), Some(1), "{}", text(&missing));
    let message = text(&missing);
    assert!(
        message.contains("model-key") && message.contains("BOT_SECRET__personal__model_key"),
        "{message}"
    );
    assert!(!message.contains("통과"), "{message}");
    // 키가 있다: 통과한다(값은 어디에도 찍지 않는다)
    let value = "value-for-the-model-key";
    let ok = bot(
        &refs(&env(&[("BOT_SECRET__personal__model_key", value)])),
        &["validate"],
    );
    assert!(ok.status.success(), "{}", text(&ok));
    assert!(text(&ok).contains("통과"), "{}", text(&ok));
    assert!(!text(&ok).contains(value));
    // 키가 있어도 모델 VM 설정이 없으면 통과하지 않는다(다른 샌드박스로 대신하지 않는다)
    let no_vm = bot(
        &refs(&env_on(
            &bare,
            &[("BOT_SECRET__personal__model_key", value)],
        )),
        &["validate"],
    );
    assert_eq!(no_vm.status.code(), Some(1), "{}", text(&no_vm));
    assert!(text(&no_vm).contains("sandbox.apple"), "{}", text(&no_vm));
    assert!(!text(&no_vm).contains("통과"), "{}", text(&no_vm));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_without_the_model_service_key_stops_before_publishing_anything() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    // 모델 CLI는 띄우지 않는다(시작 점검에서 멈춘다): 실행 파일 경로는 쓰이지 않는다
    let mut rig = Rig::launch(
        schema,
        "/nonexistent/claude".into(),
        PROFILE,
        &[],
        &cli_gateway(),
        &[Role::Worker],
        Arc::new(FakeSecrets::new(&[])),
        "",
        None,
    )
    .await;
    let error = rig.failure().await;
    assert!(error.contains("모델 서비스 키"), "{error}");
    assert!(error.contains("model-key"), "{error}");
    // 점검에서 멈췄으니 활성 판본도 시드도 예약 정의도 올라가지 않았다
    assert_eq!(rig.schema.active(), "", "게시 전에 멈췄어야 해요");
    assert_eq!(
        rig.query("select count(*) from {s}.bot_active_profiles"),
        "0"
    );
}
