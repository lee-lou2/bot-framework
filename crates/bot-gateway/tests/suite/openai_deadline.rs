//! Deadline, recheck, keep-alive, hangup, and handoff boundary tests under injected latency.
use crate::openai_support::*;
use bot_gateway::testing::api::Fault;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WITHIN: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_slow_final_check_ends_at_the_total_deadline_with_no_answer() {
    let rig = rig_with(tuned(1000, 5)).await;
    rig.api.start_as(answer("must not leak before final check"));
    // Delay active-set verification past the total deadline during slow admission.
    rig.api.fault(Fault::Hang(Duration::from_millis(150)));
    let handle = spawn_chat(&rig, BUILD_KEY, Some("slow1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.active.delay(Duration::from_secs(3));
    let started = Instant::now();
    let reply = handle.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "must respect total deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (reply.status, reply.error().1.as_str()),
        (403, "access_denied"),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("must not leak"));
}

#[tokio::test]
async fn a_slow_check_before_the_stream_opens_is_bounded_and_opens_nothing() {
    let rig = rig_with(tuned(1000, 5)).await;
    rig.api
        .start_as(answer("must not leak before stream opens"));
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
    assert!(!reply.body.contains("must not leak"));
}

#[tokio::test]
async fn the_key_is_read_again_just_before_the_stream_opens() {
    let rig = rig_with(tuned(2000, 5)).await;
    rig.api.start_as(answer("must not leak after rotation"));
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
    assert!(!reply.body.contains("must not leak"));
}

#[tokio::test]
async fn a_list_is_rechecked_after_the_slow_read_it_waited_for() {
    // Concurrent key rotation, revocation, active-set swap, or revision swap during slow bundle read.
    type Disturb = Box<dyn Fn(&Rig)>;
    let cases: Vec<(&str, Disturb, u16, &str)> = vec![
        (
            "key rotation",
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
            "member revocation",
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
            "active set swap",
            Box::new(|rig| rig.active.set(Some("set-2"))),
            503,
            "service_unavailable",
        ),
        (
            "revision swap",
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
            "{name}: stale model list must not be returned"
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
    rig.api.delay_lookups(Duration::from_secs(30));
    let mut beats = 0;
    while beats < 3 {
        match sse.next(Duration::from_secs(3)).await {
            Next::Data(chunk) if chunk == ": keep-alive\n\n" => beats += 1,
            other => panic!("expected keep-alive while lookup is stuck: {other:?}"),
        }
    }
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_hangup_during_a_stuck_lookup_is_noticed_without_waiting_for_the_next_heartbeat() {
    // With default 10s keep-alive, disconnect detection must watch hangup directly rather than waiting for a heartbeat.
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
        "disconnected stream kept waiting on stuck lookup"
    );
    assert_eq!(rig.api.runs(), 1);
}

/// Reads `n` data chunks and stops to simulate a stalled client.
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

/// Exceeds socket and hyper buffers so a stalled reader pauses body polling (18 MiB of 3-byte UTF-8).
fn huge() -> String {
    "€".repeat(6 * 1024 * 1024)
}

#[tokio::test]
async fn after_a_revocation_no_body_chunk_crosses_the_boundary_however_long_or_short_the_reader_stalled()
 {
    // Revoking membership while the reader is stalled must stop all further data handoffs and close with one error event.
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
        // Let hyper buffers fill while paused, then revoke membership.
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
            "{stall_ms}ms stall: data chunk crossed boundary after revocation"
        );
        assert_eq!(handoffs.errors(), 1, "expected exactly one error event");
        let mut error = None;
        for event in &rest {
            match data_json(event) {
                Some(value) if value.get("error").is_some() => error = Some(value),
                Some(value) => got.push_str(
                    value["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or_default(),
                ),
                None => assert_ne!(
                    event, "data: [DONE]\n\n",
                    "must not emit [DONE] after revocation"
                ),
            }
        }
        let error = error.expect("resumed stream after revocation must end with an error event");
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
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // Server deadline starts right after auth, so add slack when defining post-deadline.
    let deadline = started + Duration::from_millis(1700);
    let rest = sse.rest(Duration::from_secs(30)).await;
    assert_eq!(
        handoffs.data_after(deadline),
        (0, 0),
        "data chunk crossed boundary after deadline"
    );
    assert!(
        !rest.iter().any(|event| event == "data: [DONE]\n\n"),
        "expired stream must not emit [DONE]"
    );
    let last = rest
        .last()
        .and_then(|event| data_json(event))
        .expect("error event");
    assert_eq!(last["error"]["code"], "request_outcome_unknown");
}

#[tokio::test]
async fn a_stalled_reader_whose_recheck_is_slow_gets_nothing_more_after_the_deadline() {
    // Slow recheck after reader resumes must terminate at the total deadline without [DONE].
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
        "must not emit [DONE] without completing recheck"
    );
}

#[tokio::test]
async fn the_first_chunk_and_the_tail_are_checked_at_the_boundary_too() {
    // Revocation between pre-header check and first chunk must block even the role chunk.
    let handoffs = Handoffs::default();
    let rig = rig_with(Setup {
        probe: Some(handoffs.probe()),
        ..tuned(3000, 5)
    })
    .await;
    rig.api.start_as(answer("body"));
    // Each check reads 3 key slots; revoke after auth + pre-header check (2 * SLOTS reads).
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
        "data chunk crossed boundary after revocation: {rest:?}"
    );
    assert_eq!(handoffs.errors(), 1);
    let error = data_json(rest.last().unwrap()).expect("error event");
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
    let text: String = rest
        .iter()
        .filter_map(|chunk| data_json(chunk))
        .filter_map(|v| {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(
        text, RUNNING,
        "shutdown should end with the last known progress notice: {rest:?}"
    );
}
