//! OpenAI 호환 HTTP 입구를 **공식 SDK**(Python `openai==2.29.0`·Node `openai@6.29.0`, 해시·잠금 고정)로 끝에서 끝까지 확인한다. 제품 `serve`(실제 게이트웨이·실제 PG·실제 `claude` CLI·합성 샌드박스)를 띄우고
//! 모델 상류만 가짜로 둔다. SDK는 `base_url`·`api_key`만 바뀐 채 와이어 프록시를 거쳐 입구에 닿는다 — 프록시가 SDK 재시도까지 포함해 **실제로 간 요청**을 세고, 돌아오는 모든 응답을 공식 OpenAPI
//! (고정 커밋·sha256, 3.0 `nullable`·`$ref` 포함)로 검사한다. 서버 쪽 수(작업·모델 호출·도구 실행)는 저장소와 상류에서 센다. 키·본문은 어디에도 남기지 않는다. 실제 OpenAI API·실키는 쓰지 않는다.
//!
//! 준비: `scripts/openai-sdk-setup.sh`(공식 SDK 설치 폴더). 실행:
//! `BOT_OPENAI_CANARY=required BOT_CLAUDE_CANARY=required BOT_SRT_CANARY=required BOT_OPENAI_SDK_DIR=… BOT_TEST_DATABASE_URL=… cargo test -p bot-cli --test openai_compat -- --test-threads=1`
//! (환경이 없으면 `skipped:`를 남기고 건너뛴다. 필수 모드와 공개 관문(`INTEGRATION_TESTS`)에서는 건너뜀이 실패다.)
mod claude_support;
mod process_support;

use bot_cli::cli::Role;
use bot_store::Config;
use claude_support::*;
use serde_json::json;

/// 시험 묶음 하나(`main`: 평소 입구, `tight`: 대기 3초·실행 한도 1·본문 한도 4 KiB)를 돌린다. 돌릴 시험 파일은 `openai_sdk/expected.json`이 정한다(코드에 목록을 따로 두지 않는다) —
/// 거기 적힌 시험이 전부 통과해야 하고(건너뜀·xfail·todo·누락·미등록·통과 0건은 실패), 프록시가 본 것(공식 스키마·SSE 틀 위반 0, 요청 1건 이상, 기대한 공식 스키마를 모두 검사)도 확인한다.
/// 이어서 서버 쪽 공통 점검: 모델 대본이 남지 않았는지, 도구가 승인 없이 실행되지 않았는지, 끝나지 않은 작업이 없는지, 서버가 키를 저장소에 평문으로 남기지 않았는지.
async fn run_suite(fixture: &Fixture, sdk: &Sdk, profile: &str) {
    let proxy = sdk.proxy(fixture.port).await;
    let env = sdk.env(fixture, &proxy);
    let (python, node) = sdk.suite(profile);
    assert!(
        !python.is_empty() && !node.is_empty(),
        "{profile}: 기대 시험 파일이 비었다"
    );
    let mut failures = Vec::new();
    let mut ran = 0;
    for file in &python {
        let out = sdk.python(&env, file).await;
        ran += out.ran;
        eprintln!(
            "python {file}: {}건 {}",
            out.ran,
            if out.ok { "통과" } else { "실패" }
        );
        if !out.ok {
            failures.push(format!("python {file}\n{}", out.log));
        }
    }
    for file in &node {
        let out = sdk.node(&env, file).await;
        ran += out.ran;
        eprintln!(
            "node {file}: {}건 {}",
            out.ran,
            if out.ok { "통과" } else { "실패" }
        );
        if !out.ok {
            failures.push(format!("node {file}\n{}", out.log));
        }
    }
    let wire = sdk.check_proxy(&env, &proxy, profile).await;
    if !wire.ok {
        failures.push(format!("프록시\n{}", wire.log));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n=====\n"));
    eprintln!("{profile}: 공식 SDK 시험 {ran}건 통과");
    let stats = fixture.control.stats().await;
    assert_eq!(
        stats["queued"], 0,
        "시험이 쌓은 모델 응답이 소비되지 않고 남았다: {stats}"
    );
    assert_eq!(stats["tool_runs"], 0, "승인 없이 도구가 실행됐다: {stats}");
    assert_eq!(stats["active"], 0, "끝나지 않은 작업이 남았다: {stats}");
    assert_eq!(
        fixture.stored_secrets(),
        vec![],
        "키 값이 저장소에 평문으로 남았다"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn official_sdks_run_the_full_scenario_against_the_real_gateway() {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, sdk)) = Fixture::start(Shape::Main, Some(one_attempt)).await else {
        return;
    };
    run_suite(&fixture, &sdk, "main").await;
    fixture.rig.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn official_sdks_see_the_wait_deadline_the_saturation_limit_and_the_body_limit_of_a_tight_entrance()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, sdk)) = Fixture::start(Shape::Tight, Some(one_attempt)).await else {
        return;
    };
    run_suite(&fixture, &sdk, "tight").await;
    fixture.rig.stop().await;
}

const ROTATE_BODY: &str =
    r#"{"model":"assistant","messages":[{"role":"user","content":"rotation e2e"}]}"#;

fn models(fixture: &Fixture, key: &str) -> impl std::future::Future<Output = Reply> {
    fixture.request("GET", "/v1/models", key, None, vec![])
}

/// 키 값의 회전·철회는 다음 요청에 바로 반영된다(캐시가 갱신을 넘지 않는다): 옛 값 401·새 값 통과, 값이 없는 슬롯만 꺼지고 다른 슬롯은 그대로, 한 값이 두 슬롯에 맞으면 둘 다 거절,
/// 같은 주체의 다른 슬롯으로는 접수한 작업을 계속 조회할 수 있고 모든 슬롯이 사라지면 401이며 새 작업은 만들어지지 않는다. 어떤 값도 저장소에 평문으로 남지 않는다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn key_rotation_and_revocation_take_effect_on_the_next_request_and_never_lose_an_accepted_run()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((fixture, _sdk)) = Fixture::start(Shape::Main, Some(one_attempt)).await else {
        return;
    };
    let keys = &fixture.keys;
    let next = "sk-e2e-rotated-next-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    let again = "sk-e2e-rotated-again-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
    let status = |reply: &Reply| reply.status;
    assert_eq!(
        (
            status(&models(&fixture, &keys.build).await),
            status(&models(&fixture, &keys.rotated).await)
        ),
        (200, 200)
    );
    // 회전: 새 값은 바로 통과하고 옛 값은 바로 401, 다른 슬롯은 그대로
    fixture.secrets.set("rotated-api-key", next);
    assert_eq!(status(&models(&fixture, next).await), 200);
    let old = models(&fixture, &keys.rotated).await;
    assert_eq!(old.status, 401);
    assert!(
        !old.body.contains(&keys.rotated) && !old.body.contains(next),
        "오류는 키를 되돌려 주지 않는다"
    );
    assert_eq!(status(&models(&fixture, &keys.build).await), 200);
    // 값이 사라진 슬롯만 꺼진다
    fixture.secrets.remove("rotated-api-key");
    assert_eq!(
        (
            status(&models(&fixture, next).await),
            status(&models(&fixture, &keys.build).await)
        ),
        (401, 200)
    );
    // 한 값이 두 슬롯에 맞으면 어느 키로도 인증하지 않는다
    fixture.secrets.set("rotated-api-key", &keys.build);
    assert_eq!(
        status(&models(&fixture, &keys.build).await),
        401,
        "모호한 값은 인증하지 않는다"
    );
    assert_eq!(
        status(&models(&fixture, &keys.ops).await),
        200,
        "다른 값은 영향이 없다"
    );
    fixture.secrets.set("rotated-api-key", again);
    assert_eq!(status(&models(&fixture, &keys.build).await), 200);

    // 접수한 작업은 키가 바뀌어도(같은 주체의 다른 슬롯으로) 계속 조회할 수 있고, 주체의 슬롯이 모두 사라지면 401이다 — 새 작업은 만들어지지 않는다.
    fixture
        .control
        .script(&json!({"text": "rotation answer", "delay_ms": 6000}));
    let before = fixture.control.stats().await;
    let id = vec![("Idempotency-Key", "rotation-run-1".to_owned())];
    send_and_hang_up(
        &fixture.origin(),
        "/v1/chat/completions",
        &keys.build,
        ROTATE_BODY,
        &[("Idempotency-Key", "rotation-run-1")],
    );
    fixture.secrets.remove("build-api-key");
    let by_other_slot = fixture
        .request(
            "POST",
            "/v1/chat/completions",
            again,
            Some(ROTATE_BODY),
            id.clone(),
        )
        .await;
    assert_eq!(
        by_other_slot.status, 200,
        "같은 주체의 다른 슬롯으로 같은 작업을 조회한다: {}",
        by_other_slot.body
    );
    fixture.secrets.remove("rotated-api-key");
    let nobody = fixture
        .request(
            "POST",
            "/v1/chat/completions",
            &keys.build,
            Some(ROTATE_BODY),
            id.clone(),
        )
        .await;
    assert_eq!(nobody.status, 401, "{}", nobody.body);
    fixture.secrets.set("build-api-key", &keys.build);
    // 작업이 끝난 뒤 복구한 키로 조회하면 같은 작업의 최종 답이다
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let reply = fixture
            .request(
                "POST",
                "/v1/chat/completions",
                &keys.build,
                Some(ROTATE_BODY),
                id.clone(),
            )
            .await;
        if reply.status == 200 && reply.body.contains("rotation answer") {
            assert_eq!(reply.header("idempotency-key"), Some("rotation-run-1"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "최종 답을 받지 못했다: {} {}",
            reply.status,
            reply.body
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let after = fixture.control.stats().await;
    assert_eq!(
        after["runs"].as_u64().unwrap() - before["runs"].as_u64().unwrap(),
        1,
        "조회마다 새 작업을 만들지 않는다: {after}"
    );
    assert_eq!(after["queued"], 0, "{after}");
    // 옛 값·새 값 어느 것도 저장소에 평문으로 남지 않았다
    for value in [
        &keys.build,
        &keys.rotated,
        &keys.ops,
        &keys.guest,
        next,
        again,
    ] {
        assert_eq!(
            fixture.tables_holding(value),
            Vec::<String>::new(),
            "키 값이 저장소에 남았다"
        );
    }
    fixture.rig.stop().await;
}

/// 같은 프로필이지만 `ops` 멤버의 승인 매핑이 있다(승인자는 로컬 소유자). 운영자가 `ops`의 **마지막 키 슬롯만** 빼고 다시 배포하는 시험에 쓴다.
const PROFILE_WITH_APPROVER: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  api: [run, calendar.read, calendar.delete]
  owner: [run, calendar.read]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "http:openai/build", roles: [api]}
  - {identity: "http:openai/ops", roles: [api]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main, approvers: {"http:openai/ops": {principal: "local:owner"}}}
  - {id: secret, adapter: http, endpoint: openai, model: secret-agent, agent: main/secret}
secrets: []
policy:
  scopes: [run, calendar.read, calendar.delete]
  agents:
    main/secret: [calendar.read]
  approve: [calendar.delete]
"#;

/// 한 주체의 마지막 슬롯만 빼고 프로필·승인 매핑은 그대로 다시 배포해도 활성화는 성공하고(승인 매핑이 슬롯 없는 멤버를 가리켜도 경고일 뿐이다), 뺀 키만 401이 되며 남은 주체는 그대로 쓴다.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_last_key_slot_of_one_subject_keeps_the_activation_and_only_that_key_loses_access()
 {
    let one_attempt = Config {
        max_attempts: 1,
        ..Config::default()
    };
    let Some((mut fixture, _sdk)) =
        Fixture::start_with_profile(Shape::Main, Some(one_attempt), PROFILE_WITH_APPROVER).await
    else {
        return;
    };
    let keys = Keys::new();
    for (label, key) in [
        ("build", &keys.build),
        ("rotated", &keys.rotated),
        ("ops", &keys.ops),
    ] {
        assert_eq!(models(&fixture, key).await.status, 200, "{label}: 배포 전");
    }
    let slots = ["build", "rotated", "guest"];
    fixture
        .rig
        .reactivate_host(
            &gateway_with(Shape::Main, fixture.port, &slots),
            PROFILE_WITH_APPROVER,
            &[Role::Gateway, Role::Worker],
        )
        .await;
    assert_eq!(
        models(&fixture, &keys.ops).await.status,
        401,
        "뺀 슬롯의 키는 배포 뒤 401"
    );
    for (label, key) in [("build", &keys.build), ("rotated", &keys.rotated)] {
        assert_eq!(
            models(&fixture, key).await.status,
            200,
            "{label}: 남은 주체는 그대로"
        );
    }
    assert_eq!(
        models(&fixture, &keys.guest).await.status,
        403,
        "비멤버 키는 그대로 403"
    );
    fixture.rig.stop().await;
}
