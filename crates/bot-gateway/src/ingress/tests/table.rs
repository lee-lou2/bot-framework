use super::*;

#[tokio::test]
async fn follows_the_active_set_and_keeps_the_old_table_when_the_new_one_is_invalid() {
    let rig = rig(base());
    assert!(matches!(
        rig.ingress.admit(cli_chat("r1", "hi")).await,
        Admission::Accepted { .. }
    ));
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
    // Invalid replacement referencing an unregistered alias keeps the previous table active.
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
    // `a-desk` resolves first (counting builds) before `z-team` fails on an unregistered channel.
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
        "must not rebuild the same failed active set on every request"
    );
    catalog.replace([base().build()]);
    assert!(matches!(
        ingress.admit(cli_chat("r9", "hi")).await,
        Admission::Accepted { .. }
    ));
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    catalog.replace([invalid()]);
    for n in 0..2 {
        let result = ingress.admit(cli_chat(&format!("s{n}"), "hi")).await;
        assert!(
            matches!(result, Admission::Accepted { .. }),
            "expected previous table to remain active: {result:?}"
        );
    }
    assert_eq!(builds.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn private_inputs_are_acked_only_on_a_conclusive_outcome() {
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    let rig = rig_with(base().trigger("main", secure), |host| {
        host.limits.per_principal_per_min = Some(30)
    });
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
            .admit(slack_dm("U1", &format!("p{n}"), "secret"))
            .await;
        assert_eq!(result, Admission::Rejected(Rejection::Unavailable), "{n}");
        assert!(!result.settled());
    }
    rig.private.fail_next(Error::Invalid);
    assert!(
        rig.ingress
            .admit(slack_dm("U1", "q1", "secret"))
            .await
            .settled()
    );
    rig.private.fail_next(Error::Denied);
    assert!(
        rig.ingress
            .admit(slack_dm("U1", "q2", "secret"))
            .await
            .settled()
    );
    assert!(
        rig.store.accepted().is_empty(),
        "private inputs must never reach the normal queue"
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
        cli_chat("c1", "hello"),
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
