//! 접수 뒤 권한·키·매핑이 바뀔 때(실제 리스너): 결과·실패·안내를 내보내기 직전의 재확인이 키 값·멤버·모델 바인딩·에이전트·활성 집합을 다시 본다.
use crate::openai_support::*;
use bot_core::api::ApiState;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn revoking_the_member_while_waiting_hides_the_result_even_when_it_finished() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "rev1",
        answer("비밀 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("rev1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.catalog.replace([fixture_shaped(Shape {
        build_member: false,
        ..Shape::default()
    })
    .build()]);
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert_eq!(
        (reply.error().0.as_str(), reply.error().1.as_str()),
        ("permission_error", "access_denied")
    );
    assert!(!reply.body.contains("비밀 결과"));
    // 철회 뒤 같은 키의 재조회도 막힌다(접수 전 검사)
    let again = chat(&rig, BUILD_KEY, Some("rev1"), &ask("assistant", "q")).await;
    assert_eq!(again.status, 403);
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_model_name_reassigned_to_another_agent_never_returns_the_old_work() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "mv1",
        answer("옛 에이전트의 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("mv1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 같은 모델 이름이 다른 에이전트를 가리키게 재배포됐다
    rig.catalog.replace([fixture_shaped(Shape {
        assistant_agent: "side",
        ..Shape::default()
    })
    .build()]);
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert_eq!(reply.error().1, "model_not_found");
    assert!(!reply.body.contains("옛 에이전트"));
    // 새 키는 새 매핑으로 새 작업이다. 옛 키의 재전달은 계속 404다.
    rig.api.start_as(answer("새 에이전트의 결과"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("mv2"), &ask("assistant", "q")).await),
        "새 에이전트의 결과"
    );
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("mv1"), &ask("assistant", "q"))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn a_binding_that_changed_under_a_stored_run_is_hidden_the_same_way() {
    let rig = rig().await;
    rig.api.start_as(answer("접수 당시의 결과"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("rb1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    // 저장소의 작업이 다른 바인딩·에이전트로 접수된 것으로 남아 있다(재지정 흉내)
    rig.api.reassign("rb1", "helper", "side");
    let reply = chat(&rig, BUILD_KEY, Some("rb1"), &ask("assistant", "q")).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(!reply.body.contains("접수 당시"));
    // 모델 바인딩이 없어졌을 때도 같다
    rig.api.start_as(answer("또 다른 결과"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("rb2"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    rig.catalog.replace([fixture_shaped(Shape {
        assistant_binding: false,
        ..Shape::default()
    })
    .build()]);
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("rb2"), &ask("assistant", "q"))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn rotating_the_key_while_waiting_closes_the_delivery_but_not_the_work() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "key1",
        answer("회전 전에 시작한 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("key1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.secrets.set(
        "build-key",
        concat!("rotated-", "0123456789", "abcdef0123", "456789abc").as_bytes(),
    );
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 401, "{}", reply.body);
    assert_eq!(reply.error().1, "invalid_api_key");
    assert!(!reply.body.contains("회전 전에"));
    // 같은 주체의 다른 키(회전용 슬롯)로 같은 작업을 되찾는다 — 키가 아니라 주체에 묶인다
    let again = chat(&rig, BUILD_NEXT_KEY, Some("key1"), &ask("assistant", "q")).await;
    assert_eq!(content(&again), "회전 전에 시작한 결과");
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn an_unconfirmable_active_set_hides_the_answer_and_a_confirmed_one_returns_it() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "act1",
        answer("확인 전의 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("act1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.active.set(Some("set-2"));
    let reply = pending.await.unwrap();
    assert_eq!(
        reply.status, 403,
        "접수 뒤 권한을 확인하지 못하면 가린다: {}",
        reply.body
    );
    assert!(!reply.body.contains("확인 전의"));
    rig.active.set(Some("set-1"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("act1"), &ask("assistant", "q")).await),
        "확인 전의 결과"
    );
}

#[tokio::test]
async fn a_key_that_now_points_at_another_subject_is_refused_even_though_it_still_authenticates() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "swap1",
        answer("build의 결과"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("swap1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    // 운영자가 두 슬롯의 값을 맞바꿨다: 같은 키 값이 이제 다른 주체(ops)의 슬롯에 맞는다
    rig.secrets.set("build-key", OPS_KEY.as_bytes());
    rig.secrets.set("ops-key", BUILD_KEY.as_bytes());
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 401, "{}", reply.body);
    assert_eq!(reply.error().1, "invalid_api_key");
    assert!(!reply.body.contains("build의 결과"));
}

#[tokio::test]
async fn a_failed_run_is_not_even_confirmed_to_a_caller_whose_access_was_revoked() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "rf1",
        ApiState::Failed {
            reason: "model_error".into(),
        },
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("rf1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.catalog.replace([fixture_shaped(Shape {
        build_member: false,
        ..Shape::default()
    })
    .build()]);
    let reply = pending.await.unwrap();
    assert_eq!(
        (reply.status, reply.error().1.as_str()),
        (403, "access_denied"),
        "{}",
        reply.body
    );
    assert_eq!(reply.header("x-should-retry"), None);
}

/// 키 보관소를 읽는 도중(확인 한 번에 슬롯마다 읽는다)에 다른 프로세스가 게시한 일이 마지막 확인에 보이는가.
/// 인증이 한 바퀴(슬롯 3개)를 읽고, 내보내기 직전 확인이 두 번째 바퀴를 읽는다: 두 번째 바퀴의 첫 읽기에서 게시가 일어난다.
const SLOTS: usize = 3;

#[tokio::test]
async fn a_publication_that_lands_while_the_key_is_being_read_is_seen_by_the_last_check() {
    type Publish = Box<dyn Fn(&Rig) + Send + Sync>;
    let cases: Vec<(&str, Publish, u16, &str)> = vec![
        (
            "다른 프로세스가 새 활성 집합을 게시",
            Box::new(|rig| rig.active.set(Some("set-2"))),
            403,
            "access_denied",
        ),
        (
            "멤버 제거를 게시",
            Box::new(|rig| {
                rig.catalog.replace([fixture_shaped(Shape {
                    build_member: false,
                    ..Shape::default()
                })
                .build()]);
            }),
            403,
            "access_denied",
        ),
        (
            "같은 모델 이름의 재지정을 게시",
            Box::new(|rig| {
                rig.catalog.replace([fixture_shaped(Shape {
                    assistant_agent: "side",
                    ..Shape::default()
                })
                .build()]);
            }),
            404,
            "model_not_found",
        ),
    ];
    for (name, publish, status, code) in cases {
        let rig = Arc::new(rig().await);
        rig.api
            .start_as(answer("옛 게이트웨이가 돌려주면 안 되는 옛 답"));
        let (hook_rig, reads) = (
            rig.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let publish = Arc::new(publish);
        rig.secrets.on_read(move || {
            if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == SLOTS {
                publish(&hook_rig);
            }
        });
        // JSON: 인증(첫 바퀴) → 접수 → 내보내기 직전 확인(둘째 바퀴: 키 읽기 도중 게시)
        let reply = chat(&rig, BUILD_KEY, Some("race1"), &ask("assistant", "q")).await;
        assert_eq!(
            (reply.status, reply.error().1.as_str()),
            (status, code),
            "{name}: {}",
            reply.body
        );
        assert!(!reply.body.contains("옛 답"), "{name}: 옛 답이 나갔어요");
    }
}

#[tokio::test]
async fn losing_the_run_scope_while_waiting_hides_the_model_even_though_the_member_stays() {
    // 멤버·바인딩·에이전트는 그대로인데 역할이 더는 `run`을 주지 않는다: 내보내기 직전 확인이 «이 주체가 이 모델을 쓸 수 있는가»를 다시 본다
    for stream in [false, true] {
        let rig = rig_with(tuned(2000, 5)).await;
        finish_later(
            &rig,
            "scope1",
            answer("스코프를 잃은 뒤에 나가면 안 되는 글"),
            Duration::from_millis(300),
        );
        let body = if stream {
            ask_stream("assistant", "q", false)
        } else {
            ask("assistant", "q")
        };
        if stream {
            let mut sse = Sse::open(&rig, BUILD_KEY, Some("scope1"), &body).await;
            assert!(matches!(
                sse.next(Duration::from_secs(5)).await,
                Next::Data(_)
            ));
            rig.catalog.replace([fixture_shaped(Shape {
                build_runs: false,
                ..Shape::default()
            })
            .build()]);
            let rest = sse.rest(Duration::from_secs(5)).await;
            let text = rest.concat();
            assert!(text.contains("model_not_found"), "{text}");
            assert!(!text.contains("나가면 안 되는"), "{text}");
        } else {
            let pending = spawn_chat(&rig, BUILD_KEY, Some("scope1"), body);
            tokio::time::sleep(Duration::from_millis(100)).await;
            rig.catalog.replace([fixture_shaped(Shape {
                build_runs: false,
                ..Shape::default()
            })
            .build()]);
            let reply = pending.await.unwrap();
            assert_eq!(
                (reply.status, reply.error().1.as_str()),
                (404, "model_not_found"),
                "{}",
                reply.body
            );
            assert!(!reply.body.contains("나가면 안 되는"));
        }
    }
}
