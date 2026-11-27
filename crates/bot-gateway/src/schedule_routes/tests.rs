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
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .binding(
            "abroad",
            Adapter::SlackChannel {
                workspace: "other".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
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
        .binding(
            "compat",
            Adapter::OpenAi {
                endpoint: "openai".into(),
                model: "bot".into(),
                approvers: std::collections::BTreeMap::new(),
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
                events: bot_core::slack::default_channel_events(),
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
    for name in ["planning", "C1", "T9/C1"] {
        assert_eq!(
            channel(&routes, "abroad", name).as_deref(),
            Ok("T9/C1"),
            "{name}"
        );
    }
    assert_eq!(channel(&routes, "abroad", "T1/C1"), Err("denied"));
    assert_eq!(channel(&routes, "team", "T9/C1"), Err("denied"));
}

#[test]
fn recipients_outside_the_profiles_registered_channel_are_refused() {
    let routes = routes_for(HOST);
    for name in ["ops", "C2", "T1/C2", "T2/C1", "elsewhere", "c1"] {
        assert_eq!(channel(&routes, "team", name), Err("denied"), "{name}");
    }
    assert_eq!(channel(&routes, "nowhere", "planning"), Err("denied"));
    let planning = target("team", ConversationKind::Channel, "planning", None);
    assert_eq!(run(&routes, "work", &planning), Err("denied"));
    let ops = target("team", ConversationKind::Channel, "ops", None);
    assert_eq!(run(&routes, "work", &ops).as_deref(), Ok("T1/C2"));
    assert_eq!(run(&routes, "ghost", &planning), Err("denied"));
}

#[test]
fn dm_http_and_cli_entrances_and_non_channel_conversations_are_never_recipients() {
    let routes = routes_for(HOST);
    for binding in ["dm", "desk", "hook", "compat"] {
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
        ws("T1", r#"{"ops": "C2"}"#),
        ws("T2", r#"{"planning": "C1"}"#),
        ws("T1", r#"{"planning": "C7"}"#),
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

#[test]
fn explicit_destination_guard_only_accepts_slack_channels_with_slack_transport() {
    let channel = Adapter::SlackChannel {
        workspace: "home".into(),
        channel: "planning".into(),
        events: bot_core::slack::default_channel_events(),
    };
    assert_eq!(
        slack_destination(&channel).unwrap(),
        ("home", "planning", PushTransportId::SLACK)
    );
    for other in [
        Adapter::SlackDm {
            workspace: "home".into(),
        },
        Adapter::Cli {
            account: "me".into(),
        },
        Adapter::Http {
            endpoint: "inbox".into(),
        },
        Adapter::OpenAi {
            endpoint: "openai".into(),
            model: "bot".into(),
            approvers: std::collections::BTreeMap::new(),
        },
    ] {
        assert!(
            matches!(slack_destination(&other), Err(Error::Denied)),
            "{other:?}"
        );
    }
}
