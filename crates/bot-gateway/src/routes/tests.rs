use super::*;
use crate::testing::*;

fn host() -> HostConfig {
    HostConfig::from_json(
        r#"{
        "cli": {"accounts": {"owner": {"uid": 501}, "other": {"uid": 502}}},
        "slack": {"workspaces": {"home": {
            "team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token",
            "channels": {"planning": "C1", "ops": "C2", "planning-copy": "C1"}}}},
        "http": {"endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "k"}}}
    }"#,
    )
    .unwrap()
}

fn uid(account: &CliAccount) -> Result<u32, HostError> {
    Ok(account.uid.unwrap())
}

fn cli(account: &str) -> Adapter {
    Adapter::Cli {
        account: account.into(),
    }
}

fn channel(channel: &str) -> Adapter {
    Adapter::SlackChannel {
        workspace: "home".into(),
        channel: channel.into(),
    }
}

fn build(snapshots: Vec<Arc<Snapshot>>) -> Result<Routes, RoutesError> {
    Routes::build(catalog(snapshots), &host(), &uid)
}

#[test]
fn finds_each_entrance_by_its_resolved_identity() {
    let personal = profile("personal")
        .binding("desk", cli("owner"), "main")
        .binding("team", channel("planning"), "main")
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .build();
    let routes = build(vec![personal]).unwrap();
    let p = ProfileId("personal".into());
    let cli_key = Real::Cli {
        uid: 501,
        profile: p.clone(),
    };
    assert_eq!(routes.find(&cli_key).unwrap().binding_id.0, "desk");
    assert_eq!(
        routes
            .find(&Real::SlackChannel {
                team: "T1".into(),
                channel: "C1".into()
            })
            .unwrap()
            .binding_id
            .0,
        "team"
    );
    assert!(routes.find(&Real::SlackDm { team: "T1".into() }).is_some());
    assert!(routes.http_owner("inbox").is_some());
    // 모르는 채널·다른 사용자·다른 프로필은 입구가 아니다
    assert!(
        routes
            .find(&Real::SlackChannel {
                team: "T1".into(),
                channel: "C9".into()
            })
            .is_none()
    );
    assert!(
        routes
            .find(&Real::Cli {
                uid: 502,
                profile: p.clone()
            })
            .is_none()
    );
    assert!(
        routes
            .find(&Real::Cli {
                uid: 501,
                profile: ProfileId("work".into())
            })
            .is_none()
    );
    assert_eq!(routes.cli_profiles(501), vec![p]);
}

#[test]
fn rejects_the_same_alias_in_two_profiles_before_resolving() {
    let a = profile("a")
        .binding("x", channel("planning"), "main")
        .build();
    let b = profile("b")
        .binding("y", channel("planning"), "main")
        .build();
    let err = build(vec![a, b]).err().unwrap();
    assert!(err.0.contains("a/x") && err.0.contains("b/y"), "{err}");
}

#[test]
fn rejects_two_aliases_that_resolve_to_the_same_real_channel() {
    let a = profile("a")
        .binding("x", channel("planning"), "main")
        .build();
    let b = profile("b")
        .binding("y", channel("planning-copy"), "main")
        .build();
    let err = build(vec![a, b]).err().unwrap();
    assert!(err.0.contains("겹쳐요"), "{err}");
    assert!(!err.0.contains("C1"), "{err}");
}

#[test]
fn rejects_aliases_the_host_does_not_register() {
    let missing_channel = profile("a").binding("x", channel("nope"), "main").build();
    assert!(
        build(vec![missing_channel])
            .err()
            .unwrap()
            .0
            .contains("nope")
    );
    let missing_account = profile("a").binding("x", cli("ghost"), "main").build();
    assert!(
        build(vec![missing_account])
            .err()
            .unwrap()
            .0
            .contains("ghost")
    );
    let missing_endpoint = profile("a")
        .binding(
            "x",
            Adapter::Http {
                endpoint: "ghost".into(),
            },
            "main",
        )
        .build();
    assert!(
        build(vec![missing_endpoint])
            .err()
            .unwrap()
            .0
            .contains("ghost")
    );
}

#[test]
fn the_same_os_user_may_use_two_profiles_through_different_accounts() {
    let a = profile("a").binding("x", cli("owner"), "main").build();
    // 다른 별칭이 같은 uid를 가리켜도 프로필이 다르면 서로 다른 입구다
    let mut both = host();
    both.cli.accounts.get_mut("other").unwrap().uid = Some(501);
    let b = profile("b").binding("y", cli("other"), "main").build();
    let routes = Routes::build(catalog(vec![a, b]), &both, &uid).unwrap();
    assert_eq!(routes.cli_profiles(501).len(), 2);
}

#[test]
fn two_accounts_of_one_profile_on_the_same_os_user_are_refused_with_the_real_cause() {
    // 한 프로필의 두 바인딩이 서로 다른 계정 별칭을 쓰지만 호스트 설정에서 같은 OS 사용자로 풀린다
    let mut same_user = host();
    same_user.cli.accounts.get_mut("other").unwrap().uid = Some(501);
    let p = profile("notes")
        .binding("desk", cli("owner"), "main")
        .binding("lab", cli("other"), "main")
        .build();
    let err = Routes::build(catalog(vec![p]), &same_user, &uid)
        .err()
        .unwrap()
        .0;
    assert!(err.contains("notes/desk(cli 계정 owner)"), "{err}");
    assert!(err.contains("notes/lab(cli 계정 other)"), "{err}");
    assert!(err.contains("같은 OS 사용자"), "{err}");
    assert!(err.contains("`gateway.cli.accounts`"), "{err}");
    assert!(!err.contains("501"), "실제 uid는 싣지 않는다: {err}");
    // 별칭이 서로 달라 서로 다른 OS 사용자면 겹치지 않는다
    let ok = profile("notes")
        .binding("desk", cli("owner"), "main")
        .binding("lab", cli("other"), "main")
        .build();
    assert!(build(vec![ok]).is_ok());
}

#[test]
fn candidates_are_sorted_and_equal_orders_are_rejected() {
    let mut late = event_trigger("late", "team", tool_target("a.b", "{}"));
    late.order = 5;
    let early = event_trigger("early", "team", tool_target("a.c", "{}"));
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .trigger("main", late)
        .trigger("main", early)
        .build();
    let routes = build(vec![p]).unwrap();
    let route = routes
        .find(&Real::SlackChannel {
            team: "T1".into(),
            channel: "C1".into(),
        })
        .unwrap();
    let ids: Vec<_> = route
        .triggers
        .iter()
        .map(|t| t.trigger.id.as_str())
        .collect();
    assert_eq!(ids, ["early", "late"]);

    let same_a = event_trigger("one", "team", tool_target("a.b", "{}"));
    let same_b = event_trigger("two", "team", tool_target("a.c", "{}"));
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .trigger("main", same_a)
        .trigger("main", same_b)
        .build();
    assert!(build(vec![p]).err().unwrap().0.contains("순위가 같아"));
}

#[test]
fn rejects_unsupported_events_unknown_bindings_and_reserved_http_paths() {
    let mut odd = event_trigger("odd", "team", tool_target("a.b", "{}"));
    if let TriggerSource::Event { event, .. } = &mut odd.on {
        *event = "reaction".into();
    }
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .trigger("main", odd)
        .build();
    assert!(
        build(vec![p])
            .err()
            .unwrap()
            .0
            .contains("지원하지 않는 이벤트")
    );

    let dangling = event_trigger("t", "ghost", tool_target("a.b", "{}"));
    let p = profile("p").trigger("main", dangling).build();
    assert!(build(vec![p]).err().unwrap().0.contains("없는 바인딩"));

    let status = http_trigger(
        "t",
        "inbox",
        "GET",
        "/_requests/x",
        tool_target("a.b", "{}"),
    );
    let p = profile("p")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .trigger("main", status)
        .build();
    assert!(build(vec![p]).err().unwrap().0.contains("예약"));

    // 사건 트리거를 HTTP 바인딩에, HTTP 트리거를 채팅 바인딩에 걸 수 없다
    let wrong = event_trigger("t", "inbox", tool_target("a.b", "{}"));
    let p = profile("p")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .trigger("main", wrong)
        .build();
    assert!(build(vec![p]).err().unwrap().0.contains("맞지 않아요"));
}

fn dm_profile(extra: Vec<Trigger>, private: Trigger) -> Arc<Snapshot> {
    let mut fixture = profile("p")
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .trigger("main", private);
    for trigger in extra {
        fixture = fixture.trigger("main", trigger);
    }
    fixture.build()
}

#[test]
fn private_triggers_must_be_the_first_and_only_possible_handler() {
    let mut private = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    private.private = true;
    private.order = 10;
    assert!(build(vec![dm_profile(vec![], private.clone())]).is_ok());

    // 더 앞선(또는 같은 순위의) 일반 트리거가 있으면 거절
    let mut ahead = event_trigger("general", "direct", tool_target("a.b", "{}"));
    ahead.order = 5;
    let err = build(vec![dm_profile(vec![ahead], private.clone())])
        .err()
        .unwrap();
    assert!(err.0.contains("먼저 잡을 수"), "{err}");
    // 더 뒤의 일반 트리거는 괜찮다
    let mut behind = event_trigger("general", "direct", tool_target("a.b", "{}"));
    behind.order = 20;
    assert!(build(vec![dm_profile(vec![behind], private.clone())]).is_ok());

    // private은 DM 바인딩 · 도구 대상에서만
    let mut on_channel = private.clone();
    on_channel.on = TriggerSource::Event {
        binding: BindingId("team".into()),
        event: "message".into(),
    };
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .trigger("main", on_channel)
        .build();
    assert!(build(vec![p]).err().unwrap().0.contains("DM 바인딩"));
    let mut to_agent = private;
    to_agent.target = TriggerTarget::Instruction("x".into());
    assert!(
        build(vec![dm_profile(vec![], to_agent)])
            .err()
            .unwrap()
            .0
            .contains("도구 직접")
    );
}

#[test]
fn detects_when_the_active_set_was_replaced() {
    let active = catalog(vec![
        profile("a").binding("x", cli("owner"), "main").build(),
    ]);
    let routes = Routes::build(active.clone(), &host(), &uid).unwrap();
    assert!(routes.is_current(&active));
    let replaced = catalog(vec![
        profile("a").binding("x", cli("owner"), "main").build(),
    ]);
    assert!(!routes.is_current(&replaced));
}

#[test]
fn equal_orders_are_found_even_when_another_candidate_sits_between_them() {
    // 정렬하면 message(0) · http(0) · message(0) 순이라 같은 후보군 둘이 이웃하지 않는다
    let first = event_trigger("a-first", "team", tool_target("a.b", "{}"));
    let second = event_trigger("c-second", "team", tool_target("a.c", "{}"));
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .trigger("main", first)
        .trigger("main", second)
        .build();
    assert!(build(vec![p]).err().unwrap().0.contains("순위가 같아"));

    let mut a1 = event_trigger("a1", "team", tool_target("a.b", "{}"));
    a1.order = 3;
    let mut a2 = event_trigger("a2", "team", tool_target("a.c", "{}"));
    a2.order = 3;
    let mut http = http_trigger("b-http", "inbox", "POST", "/x", tool_target("a.d", "{}"));
    http.order = 3;
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .trigger("main", a1)
        .trigger("main", a2)
        .trigger("main", http)
        .build();
    let err = build(vec![p]).err().unwrap();
    assert!(err.0.contains("a1") && err.0.contains("a2"), "{err}");
    // 서로 다른 후보군(사건 vs HTTP)의 같은 순위는 괜찮다
    let mut ok_event = event_trigger("ev", "team", tool_target("a.b", "{}"));
    ok_event.order = 3;
    let mut ok_http = http_trigger("ht", "inbox", "POST", "/x", tool_target("a.d", "{}"));
    ok_http.order = 3;
    let p = profile("p")
        .binding("team", channel("planning"), "main")
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .trigger("main", ok_event)
        .trigger("main", ok_http)
        .build();
    assert!(build(vec![p]).is_ok());
}

#[test]
fn a_dm_binding_needs_the_operator_to_open_dm_entrances() {
    let mut closed = host();
    closed.slack.workspaces.get_mut("home").unwrap().dm = false;
    let p = profile("a")
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .build();
    let err = Routes::build(catalog(vec![p]), &closed, &uid)
        .err()
        .unwrap();
    assert!(err.0.contains("DM 입구를 열지 않았어요"), "{err}");
}
