//! 활성 집합·입구 표·민감 입력 ack·저장 경로 시험(공통 도구는 부모 모듈의 것을 쓴다).
use super::*;

#[tokio::test]
async fn follows_the_active_set_and_keeps_the_old_table_when_the_new_one_is_invalid() {
    let rig = rig(base());
    assert!(matches!(
        rig.ingress.admit(cli_chat("r1", "hi")).await,
        Admission::Accepted { .. }
    ));
    // 새 집합: 멤버를 없앤다 → 곧바로 거절
    rig.catalog.replace([profile("personal")
        .binding(
            "desk",
            Adapter::Cli {
                account: "owner".into(),
            },
            "main",
        )
        .build()]);
    assert_eq!(
        rig.ingress.admit(cli_chat("r2", "hi")).await,
        Admission::Rejected(Rejection::Unregistered)
    );
    // 호스트에 없는 별칭을 쓰는 잘못된 집합은 반영하지 않고 이전 표를 계속 쓴다
    rig.catalog.replace([base()
        .binding(
            "ghost",
            Adapter::Cli {
                account: "nobody".into(),
            },
            "main",
        )
        .build()]);
    assert_eq!(
        rig.ingress.admit(cli_chat("r3", "hi")).await,
        Admission::Rejected(Rejection::Unregistered)
    );
    // 고쳐진 집합은 다시 반영된다
    rig.catalog.replace([base().build()]);
    assert!(matches!(
        rig.ingress.admit(cli_chat("r4", "hi")).await,
        Admission::Accepted { .. }
    ));
}

#[tokio::test]
async fn without_any_valid_table_nothing_is_acked() {
    let store = MemoryStore::new();
    let catalog = TestCatalog::new([base()
        .binding(
            "ghost",
            Adapter::Cli {
                account: "nobody".into(),
            },
            "main",
        )
        .build()]);
    let ingress = Ingress::new(
        catalog,
        Arc::new(HostConfig::from_json(HOST).unwrap()),
        store.clone(),
        MemoryPrivate::new(),
    )
    .with_uid_resolver(|account| Ok(account.uid.unwrap()));
    let result = ingress.admit(cli_chat("r1", "hi")).await;
    assert_eq!(result, Admission::Rejected(Rejection::Unavailable));
    assert!(ingress.routes().is_err());
    assert!(store.accepted().is_empty());
}

#[test]
fn only_conclusive_outcomes_may_be_acked_without_redelivery() {
    let wait = Duration::from_secs(3);
    for transient in [
        Admission::Rejected(Rejection::Unavailable),
        Admission::Rejected(Rejection::RateLimited(wait)),
    ] {
        assert!(!transient.settled(), "{transient:?}");
    }
    for conclusive in [
        accepted("run-1"),
        Admission::Private,
        Admission::Ignored,
        Admission::Rejected(Rejection::Unregistered),
        Admission::Rejected(Rejection::Forbidden),
        Admission::Rejected(Rejection::TooLarge),
        Admission::Rejected(Rejection::NoRoute),
        Admission::Rejected(Rejection::Invalid),
        Admission::Rejected(Rejection::Conflict),
    ] {
        assert!(conclusive.settled(), "{conclusive:?}");
    }
}

#[tokio::test]
async fn a_failed_active_set_is_built_once_not_per_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let builds = Arc::new(AtomicUsize::new(0));
    let counter = builds.clone();
    // a-desk가 먼저 풀려 uid 해석 횟수가 곧 표를 만든 횟수다. z-team은 호스트에 없는 채널이라 실패한다.
    let invalid = || {
        profile("personal")
            .binding(
                "a-desk",
                Adapter::Cli {
                    account: "owner".into(),
                },
                "main",
            )
            .binding(
                "z-team",
                Adapter::SlackChannel {
                    workspace: "home".into(),
                    channel: "ghost".into(),
                    events: bot_core::slack::default_channel_events(),
                },
                "main",
            )
            .member("local:owner", &["owner"])
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build()
    };
    let catalog = TestCatalog::new([invalid()]);
    let ingress = Ingress::new(
        catalog.clone(),
        Arc::new(HostConfig::from_json(HOST).unwrap()),
        MemoryStore::new(),
        MemoryPrivate::new(),
    )
    .with_uid_resolver(move |account| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(account.uid.unwrap())
    });
    for n in 0..5 {
        let result = ingress.admit(cli_chat(&format!("r{n}"), "hi")).await;
        assert_eq!(result, Admission::Rejected(Rejection::Unavailable));
    }
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "같은 실패 집합을 요청마다 다시 만들면 안 돼요"
    );
    // 다른(고쳐진) 집합이 오면 다시 만들어 반영한다
    catalog.replace([base().build()]);
    assert!(matches!(
        ingress.admit(cli_chat("r9", "hi")).await,
        Admission::Accepted { .. }
    ));
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    // 이후 잘못된 집합이 와도 이전 표(멤버가 있는 집합)를 계속 쓰고, 그 실패 집합은 한 번만 만든다
    catalog.replace([invalid()]);
    for n in 0..2 {
        let result = ingress.admit(cli_chat(&format!("s{n}"), "hi")).await;
        assert!(
            matches!(result, Admission::Accepted { .. }),
            "이전 표를 쓴다: {result:?}"
        );
    }
    assert_eq!(builds.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn private_inputs_are_acked_only_on_a_conclusive_outcome() {
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    // 이 시험은 요청을 6번 보내므로 주체 한도를 넉넉히 둔다(기본 fixture는 3)
    let rig = rig_with(base().trigger("main", secure), |host| {
        host.limits.per_principal_per_min = Some(30)
    });
    // 진행 중·시작 전 재시도·충돌·장애 신호는 모두 재전달(ack 안 함)
    for (n, error) in [
        Error::Unavailable,
        Error::Stale,
        Error::Missing,
        Error::Conflict,
    ]
    .into_iter()
    .enumerate()
    {
        rig.private.fail_next(error);
        let result = rig
            .ingress
            .admit(slack_dm("U1", &format!("p{n}"), "비밀"))
            .await;
        assert_eq!(result, Admission::Rejected(Rejection::Unavailable), "{n}");
        assert!(!result.settled());
    }
    // 형식·권한 거절만 확정이다
    rig.private.fail_next(Error::Invalid);
    assert!(
        rig.ingress
            .admit(slack_dm("U1", "q1", "비밀"))
            .await
            .settled()
    );
    rig.private.fail_next(Error::Denied);
    assert!(
        rig.ingress
            .admit(slack_dm("U1", "q2", "비밀"))
            .await
            .settled()
    );
    assert!(
        rig.store.accepted().is_empty(),
        "어느 경우에도 일반 큐에는 아무것도 없다"
    );
}

#[tokio::test]
async fn http_requests_store_their_route_and_mode_and_chat_requests_do_not() {
    let fixture = base()
        .trigger(
            "main",
            sync(http_trigger(
                "agenda",
                "inbox",
                "POST",
                "/agenda",
                tool_target("agenda.delete", "{}"),
            )),
        )
        .trigger(
            "main",
            http_trigger(
                "list",
                "inbox",
                "POST",
                "/list",
                tool_target("agenda.list", "{}"),
            ),
        );
    let rig = rig_with(fixture, |host| host.limits.per_principal_per_min = Some(30));
    for inbound in [
        http_post("http:inbox/svc", "r1", "/agenda", json!({})),
        http_post("http:inbox/svc", "r2", "/list", json!({})),
        cli_chat("c1", "안녕"),
    ] {
        assert!(matches!(
            rig.ingress.admit(inbound).await,
            Admission::Accepted { .. }
        ));
    }
    let records = rig.store.accepted();
    assert_eq!(
        records[0].route,
        Some(HttpRoute {
            method: "POST".into(),
            path: "/agenda".into(),
            response: ResponseMode::Sync
        })
    );
    assert_eq!(
        records[1].route.as_ref().map(|route| route.response),
        Some(ResponseMode::Accepted)
    );
    assert_eq!(records[2].route, None);
}
