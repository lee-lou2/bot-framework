//! 기다림·확인이 느릴 때의 기한과 재확인(실제 리스너, 지연 주입): 내보내기 직전 확인도 총 기한 안에 끝나야 하고, 기다리는 동안 읽은 값은 기다림이 끝난 뒤 다시 확인하며,
//! 스트림은 조회가 멈춰도 keep-alive를 쓰고 끊김을 알아채며, 상대가 읽기를 멈췄다 풀어도 철회된 뒤에는 새 글을 쓰지 않는다.
use bot_gateway::testing::api::Fault;
use openai_support::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod openai_support;

const WITHIN: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_slow_final_check_ends_at_the_total_deadline_with_no_answer() {
    let rig = rig_with(tuned(1000, 5)).await;
    rig.api
        .start_as(answer("확인이 끝나기 전에 나가면 안 되는 글"));
    // 접수가 잠깐 걸리는 사이 저장소가 느려진다: 내보내기 직전 활성 집합 확인만 기한보다 오래 걸린다
    rig.api.fault(Fault::Hang(Duration::from_millis(150)));
    let handle = spawn_chat(&rig, BUILD_KEY, Some("slow1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.active.delay(Duration::from_secs(3));
    let started = Instant::now();
    let reply = handle.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "총 기한을 넘기지 않는다: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (reply.status, reply.error().1.as_str()),
        (403, "access_denied"),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("나가면 안 되는"));
}

#[tokio::test]
async fn a_slow_check_before_the_stream_opens_is_bounded_and_opens_nothing() {
    let rig = rig_with(tuned(1000, 5)).await;
    rig.api
        .start_as(answer("스트림 전 확인이 느리면 나가면 안 되는 글"));
    rig.api.fault(Fault::Hang(Duration::from_millis(100)));
    let handle = spawn_chat(
        &rig,
        BUILD_KEY,
        Some("slow2"),
        ask_stream("assistant", "q", false),
    );
    tokio::time::sleep(Duration::from_millis(60)).await;
    rig.active.delay(Duration::from_secs(3));
    let started = Instant::now();
    let reply = handle.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        (reply.status, reply.header("content-type")),
        (403, Some("application/json")),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("나가면 안 되는"));
}

#[tokio::test]
async fn the_key_is_read_again_just_before_the_stream_opens() {
    let rig = rig_with(tuned(2000, 5)).await;
    rig.api.start_as(answer("회전 뒤에 나가면 안 되는 글"));
    // 접수가 느리다: 인증 뒤 접수가 끝나기 전에 키가 회전된다
    rig.api.fault(Fault::Hang(Duration::from_millis(300)));
    let handle = spawn_chat(
        &rig,
        BUILD_KEY,
        Some("rk1"),
        ask_stream("assistant", "q", false),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.secrets.set(
        "build-key",
        concat!("rotated-", "0123456789", "abcdef0123", "456789abc").as_bytes(),
    );
    let reply = handle.await.unwrap();
    assert_eq!(
        (reply.status, reply.header("content-type")),
        (401, Some("application/json")),
        "{}",
        reply.body
    );
    assert_eq!(reply.error().1, "invalid_api_key");
    assert!(!reply.body.contains("나가면 안 되는"));
}

#[tokio::test]
async fn a_list_is_rechecked_after_the_slow_read_it_waited_for() {
    // 판본 번들 시각을 읽는 동안 키 회전·멤버 철회·집합 교체·판본 교체가 끼어든다: 옛 표로 만든 목록이 나가지 않는다
    type Disturb = Box<dyn Fn(&Rig)>;
    let cases: Vec<(&str, Disturb, u16, &str)> = vec![
        (
            "키 회전",
            Box::new(|rig| {
                rig.secrets.set(
                    "build-key",
                    concat!("rotated-", "0123456789", "abcdef0123", "456789abc").as_bytes(),
                )
            }),
            401,
            "invalid_api_key",
        ),
        (
            "멤버 철회",
            Box::new(|rig| {
                rig.catalog.replace([fixture_shaped(Shape {
                    build_member: false,
                    ..Shape::default()
                })
                .build()])
            }),
            403,
            "access_denied",
        ),
        (
            "다른 프로세스가 집합 교체",
            Box::new(|rig| rig.active.set(Some("set-2"))),
            503,
            "service_unavailable",
        ),
        (
            "판본 교체",
            Box::new(|rig| {
                rig.catalog
                    .replace([fixture().revision("personal-r2").build()])
            }),
            503,
            "service_unavailable",
        ),
    ];
    for (name, disturb, status, code) in cases {
        let rig = rig().await;
        rig.bundles.delay(Duration::from_millis(400));
        let addr = rig.addr;
        let pending = tokio::spawn(async move {
            send(
                addr,
                "GET",
                "/v1/models",
                &[("Authorization", &bearer(BUILD_KEY))],
                "",
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        disturb(&rig);
        let reply = pending.await.unwrap();
        assert_eq!(
            (reply.status, reply.error().1.as_str()),
            (status, code),
            "{name}: {}",
            reply.body
        );
        assert!(
            !reply.body.contains("assistant"),
            "{name}: 옛 표의 모델이 나가면 안 된다"
        );
    }
}

#[tokio::test]
async fn an_undisturbed_slow_list_still_answers() {
    let rig = rig().await;
    rig.bundles.delay(Duration::from_millis(300));
    let reply = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["data"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn a_stuck_store_cannot_hold_a_list_request_forever() {
    let rig = rig_with(Setup {
        models_budget: Some(Duration::from_millis(400)),
        ..Setup::default()
    })
    .await;
    rig.bundles.delay(Duration::from_secs(60));
    let started = Instant::now();
    let reply = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(
        (reply.status, reply.error().1.as_str()),
        (503, "service_unavailable"),
        "{}",
        reply.body
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_stuck_lookup_does_not_stop_the_stream_from_breathing_or_noticing_a_hangup() {
    let rig = rig_with(Setup {
        keep_alive: Some(Duration::from_millis(150)),
        ..tuned(6000, 5)
    })
    .await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("stuck1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(chunk) if chunk.contains("assistant")));
    // 이제부터 조회가 멈춘다(저장소가 응답하지 않는다)
    rig.api.delay_lookups(Duration::from_secs(30));
    let mut beats = 0;
    while beats < 3 {
        match sse.next(Duration::from_secs(3)).await {
            Next::Data(chunk) if chunk == ": keep-alive\n\n" => beats += 1,
            other => panic!("조회가 멈춰도 keep-alive가 와야 해요: {other:?}"),
        }
    }
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_hangup_during_a_stuck_lookup_is_noticed_without_waiting_for_the_next_heartbeat() {
    // keep-alive는 기본(10초)이다: 다음 주석을 쓰다 끊김을 알아채는 길로는 이 시간 안에 알 수 없다. 끊김 자체를 지켜보아야 멈춘 조회를 곧바로 버린다.
    let rig = rig_with(tuned(8000, 5)).await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("stuck3"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(chunk) if chunk.contains("assistant")));
    rig.api.delay_lookups(Duration::from_secs(60));
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(sse);
    let until = Instant::now() + Duration::from_secs(2);
    while rig.api.dropped_lookups() == 0 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        rig.api.dropped_lookups() >= 1,
        "끊긴 스트림이 멈춘 조회를 계속 기다려요"
    );
    assert_eq!(rig.api.runs(), 1);
}

/// 헤더 끝까지 읽고 `n`개의 데이터 청크를 읽은 뒤 멈춘다(상대가 읽지 않는 상황)
async fn read_some(sse: &mut Sse, n: usize) -> String {
    let mut text = String::new();
    for _ in 0..n {
        match sse.next(WITHIN).await {
            Next::Data(chunk) => {
                if let Some(value) = data_json(&chunk) {
                    text.push_str(
                        value["choices"][0]["delta"]["content"]
                            .as_str()
                            .unwrap_or_default(),
                    );
                }
            }
            other => panic!("{other:?}"),
        }
    }
    text
}

/// 소켓 버퍼·hyper 버퍼를 한참 넘는 긴 글: 상대가 읽지 않으면 조각을 꺼내 가는 쪽(본문)이 멈춘다
fn huge() -> String {
    "가".repeat(6 * 1024 * 1024)
}

#[tokio::test]
async fn after_a_revocation_no_body_chunk_crosses_the_boundary_however_long_or_short_the_reader_stalled()
 {
    // 상대가 읽기를 멈춘 사이 멤버가 철회된다. 재개한 뒤에는 그 순간 이후 **한 조각도** 본문으로 넘어가지 않고(관찰 지점이 센다) 오류 사건 하나로 닫힌다.
    for stall_ms in [250u64, 1500] {
        let handoffs = Handoffs::default();
        let rig = rig_with(Setup {
            probe: Some(handoffs.probe()),
            ..tuned(8000, 5)
        })
        .await;
        let text = huge();
        rig.api.start_as(answer(&text));
        let mut sse = Sse::open(
            &rig,
            BUILD_KEY,
            Some("stall1"),
            &ask_stream("assistant", "q", false),
        )
        .await;
        let mut got = read_some(&mut sse, 2).await;
        // 읽기를 멈춘다: 본문이 hyper 버퍼를 채우고 멈추도록 잠시 둔 뒤 철회한다
        tokio::time::sleep(Duration::from_millis(400)).await;
        rig.catalog.replace([fixture_shaped(Shape {
            build_member: false,
            ..Shape::default()
        })
        .build()]);
        let revoked_at = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(stall_ms)).await;
        let rest = sse.rest(Duration::from_secs(30)).await;
        assert_eq!(
            handoffs.data_after(revoked_at),
            (0, 0),
            "{stall_ms}ms 정체: 철회 뒤에 본문 조각이 넘어갔어요"
        );
        assert_eq!(handoffs.errors(), 1, "오류 사건은 정확히 하나다");
        let mut error = None;
        for event in &rest {
            match data_json(event) {
                Some(value) if value.get("error").is_some() => error = Some(value),
                Some(value) => got.push_str(
                    value["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or_default(),
                ),
                None => assert_ne!(event, "data: [DONE]\n\n", "철회됐는데 정상 종료가 나갔어요"),
            }
        }
        let error = error.expect("철회 뒤 재개한 스트림은 오류 사건으로 끝나야 해요");
        assert_eq!(error["error"]["code"], "access_denied");
        assert!(got.len() < text.len(), "{}/{}", got.len(), text.len());
    }
}

#[tokio::test]
async fn after_the_deadline_no_body_chunk_crosses_the_boundary() {
    let handoffs = Handoffs::default();
    let rig = rig_with(Setup {
        probe: Some(handoffs.probe()),
        ..tuned(1500, 5)
    })
    .await;
    let text = huge();
    rig.api.start_as(answer(&text));
    let started = std::time::Instant::now();
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("stall2"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let _ = read_some(&mut sse, 2).await;
    // 총 기한(1.5초)을 넘겨 읽지 않는다
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // 서버의 기한은 인증 직후에 시작하므로 요청을 보낸 때보다 조금 늦다: 여유를 두고 «기한 뒤»를 잡는다
    let deadline = started + Duration::from_millis(1700);
    let rest = sse.rest(Duration::from_secs(30)).await;
    assert_eq!(
        handoffs.data_after(deadline),
        (0, 0),
        "기한 뒤에 본문 조각이 넘어갔어요"
    );
    assert!(
        !rest.iter().any(|event| event == "data: [DONE]\n\n"),
        "기한을 넘긴 스트림은 정상 종료로 끝날 수 없어요"
    );
    let last = rest
        .last()
        .and_then(|event| data_json(event))
        .expect("오류 사건");
    assert_eq!(last["error"]["code"], "request_outcome_unknown");
}

#[tokio::test]
async fn a_stalled_reader_whose_recheck_is_slow_gets_nothing_more_after_the_deadline() {
    // 읽기를 멈췄다 푼 뒤의 재확인이 느리다(저장소 지연 + 철회): 확인을 마치지 못하면 총 기한에서 끊고 정상 종료를 쓰지 않는다
    let handoffs = Handoffs::default();
    let rig = rig_with(Setup {
        probe: Some(handoffs.probe()),
        ..tuned(3000, 5)
    })
    .await;
    let text = huge();
    rig.api.start_as(answer(&text));
    let started = Instant::now();
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("slow3"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let _ = read_some(&mut sse, 2).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.active.delay(Duration::from_secs(10));
    rig.catalog.replace([fixture_shaped(Shape {
        build_member: false,
        ..Shape::default()
    })
    .build()]);
    let revoked_at = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let rest = sse.rest(Duration::from_secs(20)).await;
    assert!(
        started.elapsed() < Duration::from_millis(5500),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(handoffs.data_after(revoked_at), (0, 0));
    assert!(
        !rest.iter().any(|event| event == "data: [DONE]\n\n"),
        "확인하지 못했는데 정상 종료가 나갔어요"
    );
}

#[tokio::test]
async fn the_first_chunk_and_the_tail_are_checked_at_the_boundary_too() {
    // 철회가 첫 청크를 꺼내기 전에 일어나도(헤더 확인과 첫 청크 사이) 역할 청크조차 넘어가지 않는다
    let handoffs = Handoffs::default();
    let rig = rig_with(Setup {
        probe: Some(handoffs.probe()),
        ..tuned(3000, 5)
    })
    .await;
    rig.api.start_as(answer("본문"));
    // 키 보관소는 확인 한 번에 슬롯마다(3개) 읽는다: 인증 한 바퀴, 헤더 전 확인 한 바퀴를 지난 뒤(첫 조각을 꺼내는 확인의 첫 읽기부터) 철회된 상태로 만든다
    const SLOTS: usize = 3;
    let (catalog, reads) = (
        rig.catalog.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    rig.secrets.on_read(move || {
        if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2 * SLOTS {
            catalog.replace([fixture_shaped(Shape {
                build_member: false,
                ..Shape::default()
            })
            .build()]);
        }
    });
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("first1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let rest = sse.rest(WITHIN).await;
    assert_eq!(
        handoffs.data_total(),
        0,
        "철회됐는데 본문 조각이 넘어갔어요: {rest:?}"
    );
    assert_eq!(handoffs.errors(), 1);
    let error = data_json(rest.last().unwrap()).expect("오류 사건");
    assert!(
        ["access_denied", "model_not_found"].contains(&error["error"]["code"].as_str().unwrap()),
        "{error}"
    );
}

#[tokio::test]
async fn shutting_down_ends_a_stream_even_while_a_lookup_is_stuck() {
    let rig = rig_with(tuned(8000, 5)).await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("stuck2"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(_)));
    rig.api.delay_lookups(Duration::from_secs(60));
    tokio::time::sleep(Duration::from_millis(900)).await;
    let started = Instant::now();
    rig.stop.cancel();
    let rest = sse.rest(Duration::from_secs(5)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        rest.concat().contains("작업을 처리 중입니다."),
        "마지막으로 확인한 상태로 안내하고 끝낸다: {rest:?}"
    );
}
