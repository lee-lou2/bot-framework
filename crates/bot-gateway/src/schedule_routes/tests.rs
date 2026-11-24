use super::*;
use crate::testing::{TestCatalog, profile};
use bot_core::{id::BindingId, profile::Adapter};

const HOST: &str = r#"{
  "cli": {"accounts": {"me": {"uid": 501}}},
  "slack": {"workspaces": {
    "home": {"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token", "dm": true,
             "channels": {"planning": "C1", "ops": "C2"}},
    "other": {"team_id": "T9", "bot_token": "other-token", "app_token": "other-app",
              "channels": {"planning": "C1"}}}}}"#;

fn routes_for(host: &str) -> HostRoutes {
    let personal = profile("personal")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
            },
            "main",
        )
        // 다른 팀(T9)의 같은 채널 ID(C1)
        .binding(
            "abroad",
            Adapter::SlackChannel {
                workspace: "other".into(),
                channel: "planning".into(),
            },
            "main",
        )
        .binding(
            "dm",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "desk",
            Adapter::Cli {
                account: "me".into(),
            },
            "main",
        )
        .binding(
            "hook",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .build();
    let work = profile("work")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "ops".into(),
            },
            "main",
        )
        .build();
    HostRoutes::new(
        TestCatalog::new([personal, work]),
        Arc::new(HostConfig::from_json(host).unwrap()),
    )
}

fn target(binding: &str, kind: ConversationKind, id: &str, thread: Option<&str>) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId(binding.into()),
        conversation: Conversation {
            kind,
            id: id.into(),
            thread: thread.map(str::to_owned),
        },
    }
}

/// 결과를 비교하기 쉬운 값으로: 확정된 `conversation.id` 또는 오류 이름
fn run(
    routes: &HostRoutes,
    profile: &str,
    wanted: &DeliveryTarget,
) -> Result<String, &'static str> {
    routes
        .resolve(&ProfileId(profile.into()), wanted)
        .map(|got| got.conversation.id)
        .map_err(|error| match error {
            Error::Invalid => "invalid",
            Error::Denied => "denied",
            _ => "other",
        })
}

fn channel(routes: &HostRoutes, binding: &str, id: &str) -> Result<String, &'static str> {
    run(
        routes,
        "personal",
        &target(binding, ConversationKind::Channel, id, None),
    )
}

#[test]
fn an_alias_a_channel_id_and_the_canonical_path_all_pin_the_same_team_and_channel() {
    let routes = routes_for(HOST);
    for name in ["planning", "C1", "T1/C1"] {
        assert_eq!(
            channel(&routes, "team", name).as_deref(),
            Ok("T1/C1"),
            "{name}"
        );
    }
    // 바인딩·종류·스레드는 그대로 둔다
    let wanted = target(
        "team",
        ConversationKind::Channel,
        "planning",
        Some("1700000000.000100"),
    );
    let got = routes
        .resolve(&ProfileId("personal".into()), &wanted)
        .unwrap();
    assert_eq!(got.binding, wanted.binding);
    assert_eq!(got.conversation.kind, ConversationKind::Channel);
    assert_eq!(got.conversation.id, "T1/C1");
    assert_eq!(
        got.conversation.thread.as_deref(),
        Some("1700000000.000100")
    );
}

#[test]
fn the_team_follows_the_binding_so_the_same_channel_id_of_another_team_is_refused() {
    let routes = routes_for(HOST);
    // `abroad`는 다른 팀(T9)의 채널 C1이다: 채널 ID만 주면 그 팀으로 고정된다
    for name in ["planning", "C1", "T9/C1"] {
        assert_eq!(
            channel(&routes, "abroad", name).as_deref(),
            Ok("T9/C1"),
            "{name}"
        );
    }
    // 다른 팀의 정규 경로는 이 바인딩의 것이 아니다(양쪽 모두)
    assert_eq!(channel(&routes, "abroad", "T1/C1"), Err("denied"));
    assert_eq!(channel(&routes, "team", "T9/C1"), Err("denied"));
}

#[test]
fn recipients_outside_the_profiles_registered_channel_are_refused() {
    let routes = routes_for(HOST);
    // 같은 워크스페이스의 다른 채널·팀이 다른 정규 경로·모르는 이름
    for name in ["ops", "C2", "T1/C2", "T2/C1", "elsewhere", "c1"] {
        assert_eq!(channel(&routes, "team", name), Err("denied"), "{name}");
    }
    // 이 프로필에 없는 바인딩
    assert_eq!(channel(&routes, "nowhere", "planning"), Err("denied"));
    // 프로필마다 자기 바인딩만 본다: work의 team은 ops 채널이다
    let planning = target("team", ConversationKind::Channel, "planning", None);
    assert_eq!(run(&routes, "work", &planning), Err("denied"));
    let ops = target("team", ConversationKind::Channel, "ops", None);
    assert_eq!(run(&routes, "work", &ops).as_deref(), Ok("T1/C2"));
    // 활성 집합에 없는 프로필
    assert_eq!(run(&routes, "ghost", &planning), Err("denied"));
}

#[test]
fn dm_http_and_cli_entrances_and_non_channel_conversations_are_never_recipients() {
    let routes = routes_for(HOST);
    // DM(수신자를 하나로 정할 수 없다)·CLI·HTTP 바인딩은 어떤 종류·ID로도 거절
    for binding in ["dm", "desk", "hook"] {
        for kind in [ConversationKind::Channel, ConversationKind::Direct] {
            for id in ["D1", "C1", "T1/C1", "planning"] {
                let got = run(
                    &routes,
                    "personal",
                    &target(binding, kind.clone(), id, None),
                );
                assert_eq!(got, Err("denied"), "{binding} {id}");
            }
        }
    }
    // 채널 바인딩이어도 대화 종류가 채널이 아니면 거절
    for kind in [
        ConversationKind::Direct,
        ConversationKind::Cli,
        ConversationKind::Http,
        ConversationKind::Scheduled,
    ] {
        let wanted = target("team", kind, "planning", None);
        assert_eq!(run(&routes, "personal", &wanted), Err("denied"));
    }
}

#[test]
fn malformed_input_is_invalid_before_the_registry_is_asked() {
    let routes = routes_for(HOST);
    for id in ["", "T1/C1\n", "T1/\u{7f}C1"] {
        assert_eq!(channel(&routes, "team", id), Err("invalid"), "{id:?}");
    }
    assert_eq!(channel(&routes, "team", &"C".repeat(80)), Err("invalid"));
    // 정규 모양이 아닌 경로는 모르는 이름이다
    for id in ["T1/", "/C1", "T1/C1/C2", "t1/c1", "T1/C-1", "a b"] {
        assert_eq!(channel(&routes, "team", id), Err("denied"), "{id:?}");
    }
    for thread in ["", "1", "1700000000", "x.y", "1.2.3", "1700000000.000100x"] {
        let wanted = target("team", ConversationKind::Channel, "planning", Some(thread));
        assert_eq!(
            run(&routes, "personal", &wanted),
            Err("invalid"),
            "{thread:?}"
        );
    }
}

#[test]
fn a_pinned_path_stops_resolving_when_the_registry_changes() {
    let pinned = "T1/C1";
    assert_eq!(
        channel(&routes_for(HOST), "team", pinned).as_deref(),
        Ok(pinned)
    );
    let ws = |team: &str, channels: &str| {
        format!(
            r#"{{"slack": {{"workspaces": {{"home": {{"team_id": "{team}", "bot_token": "b", "app_token": "a", "dm": true, "channels": {channels}}}}}}}}}"#
        )
    };
    let cases = [
        // 채널을 뺐다
        ws("T1", r#"{"ops": "C2"}"#),
        // 팀이 바뀌었다
        ws("T2", r#"{"planning": "C1"}"#),
        // 별칭이 다른 채널을 가리킨다
        ws("T1", r#"{"planning": "C7"}"#),
        // 워크스페이스를 통째로 뺐다
        r#"{"slack": {"workspaces": {}}}"#.to_owned(),
        "{}".to_owned(),
    ];
    for host in cases {
        assert_eq!(
            channel(&routes_for(&host), "team", pinned),
            Err("denied"),
            "{host}"
        );
    }
}

#[test]
fn the_route_helpers_only_accept_slack_shaped_paths_and_thread_timestamps() {
    assert_eq!(split_route("T1/C1"), Some(("T1", "C1")));
    assert_eq!(split_route("C1"), None);
    for bad in ["T1/", "/C1", "T1/C1/C2", "t1/C1", "T1/C-1", ""] {
        assert_eq!(split_route(bad), None, "{bad:?}");
    }
    assert!(is_ts("1700000000.000100") && !is_ts("1700000000") && !is_ts(""));
}
