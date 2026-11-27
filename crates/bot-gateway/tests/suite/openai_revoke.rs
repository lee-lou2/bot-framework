//! Post-admission revocation and recheck tests for OpenAI-compatible HTTP responses.
use crate::openai_support::*;
use bot_core::api::ApiState;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn revoking_the_member_while_waiting_hides_the_result_even_when_it_finished() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "rev1",
        answer("secret result"),
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
    assert!(!reply.body.contains("secret result"));
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
        answer("old agent result"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("mv1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.catalog.replace([fixture_shaped(Shape {
        assistant_agent: "side",
        ..Shape::default()
    })
    .build()]);
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert_eq!(reply.error().1, "model_not_found");
    assert!(!reply.body.contains("old agent"));
    rig.api.start_as(answer("new agent result"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("mv2"), &ask("assistant", "q")).await),
        "new agent result"
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
    rig.api.start_as(answer("admitted result"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("rb1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    rig.api.reassign("rb1", "helper", "side");
    let reply = chat(&rig, BUILD_KEY, Some("rb1"), &ask("assistant", "q")).await;
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert!(!reply.body.contains("admitted"));
    rig.api.start_as(answer("another result"));
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
        answer("started before rotation"),
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
    assert!(!reply.body.contains("started before"));
    // Runs belong to the subject, so the rotation slot key recovers the same run.
    let again = chat(&rig, BUILD_NEXT_KEY, Some("key1"), &ask("assistant", "q")).await;
    assert_eq!(content(&again), "started before rotation");
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn an_unconfirmable_active_set_hides_the_answer_and_a_confirmed_one_returns_it() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "act1",
        answer("pre-confirmation result"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("act1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.active.set(Some("set-2"));
    let reply = pending.await.unwrap();
    assert_eq!(
        reply.status, 403,
        "unconfirmed post-admission release must deny access: {}",
        reply.body
    );
    assert!(!reply.body.contains("pre-confirmation"));
    rig.active.set(Some("set-1"));
    assert_eq!(
        content(&chat(&rig, BUILD_KEY, Some("act1"), &ask("assistant", "q")).await),
        "pre-confirmation result"
    );
}

#[tokio::test]
async fn a_key_that_now_points_at_another_subject_is_refused_even_though_it_still_authenticates() {
    let rig = rig_with(tuned(2000, 5)).await;
    finish_later(
        &rig,
        "swap1",
        answer("build result"),
        Duration::from_millis(300),
    );
    let pending = spawn_chat(&rig, BUILD_KEY, Some("swap1"), ask("assistant", "q"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Swapping slot secrets moves BUILD_KEY to `ops`; recheck must refuse delivery for `build`'s run.
    rig.secrets.set("build-key", OPS_KEY.as_bytes());
    rig.secrets.set("ops-key", BUILD_KEY.as_bytes());
    let reply = pending.await.unwrap();
    assert_eq!(reply.status, 401, "{}", reply.body);
    assert_eq!(reply.error().1, "invalid_api_key");
    assert!(!reply.body.contains("build result"));
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

/// Number of OpenAI key slots read per auth/recheck pass; pass 2 read 0 triggers mid-recheck mutations.
const SLOTS: usize = 3;

#[tokio::test]
async fn a_publication_that_lands_while_the_key_is_being_read_is_seen_by_the_last_check() {
    type Publish = Box<dyn Fn(&Rig) + Send + Sync>;
    let cases: Vec<(&str, Publish, u16, &str)> = vec![
        (
            "other process publishes new active set",
            Box::new(|rig| rig.active.set(Some("set-2"))),
            403,
            "access_denied",
        ),
        (
            "member removal published",
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
            "model name reassignment published",
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
            .start_as(answer("stale reply that old gateway must not return"));
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
        let reply = chat(&rig, BUILD_KEY, Some("race1"), &ask("assistant", "q")).await;
        assert_eq!(
            (reply.status, reply.error().1.as_str()),
            (status, code),
            "{name}: {}",
            reply.body
        );
        assert!(
            !reply.body.contains("stale reply"),
            "{name}: leaked stale reply"
        );
    }
}

#[tokio::test]
async fn losing_the_run_scope_while_waiting_hides_the_model_even_though_the_member_stays() {
    for stream in [false, true] {
        let rig = rig_with(tuned(2000, 5)).await;
        finish_later(
            &rig,
            "scope1",
            answer("must not leak after losing scope"),
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
            assert!(!text.contains("must not leak"), "{text}");
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
            assert!(!reply.body.contains("must not leak"));
        }
    }
}
