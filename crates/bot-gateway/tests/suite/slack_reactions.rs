//! Slack reaction ingress, live human-account checks (`users.info`), reply threading, and pass-chain construction.
use crate::support::{events::*, *};
use bot_core::{
    pass::Candidate,
    slack::{SlackEvent, SourceKind},
};
use bot_gateway::testing::{
    slack::{Scripted, mention_frame, reaction_frame},
    *,
};
use serde_json::{Value, json};

#[tokio::test]
async fn a_reaction_by_a_member_reaches_its_trigger_with_fixed_fields_and_the_actor_as_requester() {
    let rig = connected(vec![
        channel_profile(&[SlackEvent::Mention, SlackEvent::Reaction])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    push_all(
        &rig,
        vec![
            reaction_frame("R1", TC1, "U1", "100.1", "white_check_mark", "U9"),
            reaction_frame("R1", TC1, "U1", "100.1", "white_check_mark", "U9"),
            reaction_frame("R2", TC1, "U1", "100.1", "eyes", "U9"),
            reaction_frame("R3", TC1, "U9", "100.1", "white_check_mark", "U1"),
            reaction_frame("R4", TC1, "UBOT", "100.1", "white_check_mark", "U1"),
            reaction_frame("R5", ("T1", "C9"), "U1", "100.1", "white_check_mark", "U9"),
            reaction_frame("R6", ("T9", "C1"), "U1", "100.1", "white_check_mark", "U9"),
            reaction_frame("R7", ("T1", "D1"), "U1", "100.1", "white_check_mark", "U9"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.target, "tool main agenda.list {}");
    assert_eq!(
        r.key.principal.0, "slack:home/U1",
        "requester must be the reacting user, not the item author"
    );
    assert_eq!(r.key.request.0, "Ev-R1");
    assert_eq!(
        r.conversation, "channel:C1:",
        "unproven posts default to top-level channel rather than assuming a thread"
    );
    let body: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(
        body,
        json!({"reaction": "white_check_mark", "channel": "C1", "item_ts": "100.1", "item_user": "U9"})
    );
    let source = r.source.clone().unwrap();
    assert_eq!(
        (
            source.kind,
            source.event_id.as_str(),
            source.reaction.as_deref(),
            source.ts.as_str()
        ),
        (
            SourceKind::Reaction,
            "Ev-R1",
            Some("white_check_mark"),
            "100.1"
        )
    );
}

#[tokio::test]
async fn the_bots_own_reaction_is_never_accepted_even_if_the_bot_is_a_member() {
    let rig = connected(vec![
        channel_profile(&[SlackEvent::Mention, SlackEvent::Reaction])
            .member("slack:home/UBOT", &["owner"])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    push_all(
        &rig,
        vec![
            reaction_frame("R1", TC1, "UBOT", "100.1", "white_check_mark", "U1"),
            reaction_frame("R2", TC1, "U1", "100.1", "white_check_mark", "UBOT"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].key.request.0, "Ev-R2");
}

#[tokio::test]
async fn reactions_are_ignored_unless_the_binding_enables_them_and_a_trigger_wants_them() {
    let off = connected(vec![
        channel_profile(&[SlackEvent::Mention])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    push_all(
        &off,
        vec![reaction_frame(
            "R1",
            TC1,
            "U1",
            "100.1",
            "white_check_mark",
            "U9",
        )],
    )
    .await;
    assert!(off.store.accepted().is_empty());
    let none = connected(vec![channel_profile(&[
        SlackEvent::Mention,
        SlackEvent::Reaction,
    ])])
    .await;
    push_all(
        &none,
        vec![reaction_frame(
            "R1",
            TC1,
            "U1",
            "100.1",
            "white_check_mark",
            "U9",
        )],
    )
    .await;
    assert!(none.store.accepted().is_empty());
}

#[tokio::test]
async fn the_pass_chain_lists_matching_direct_triggers_in_order_and_the_default_conversation() {
    let mut second = reaction_trigger("second", 10, "agenda.find");
    second.conditions.clear();
    let rig = connected(vec![
        channel_profile(&ALL)
            .trigger("main", reaction_trigger("first", 0, "agenda.list"))
            .trigger("main", second)
            .trigger("main", {
                let mut t = event_trigger("answer", "team", tool_target("agenda.list", "{}"));
                t.order = 0;
                t
            }),
    ])
    .await;
    push_all(
        &rig,
        vec![
            reaction_frame("R1", TC1, "U1", "100.1", "white_check_mark", "U9"),
            reaction_frame("R2", TC1, "U1", "100.2", "eyes", "U9"),
            mention_frame("E1", "T1", "C1", "U1", "200.1", "<@UBOT> question"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    let by_request = |request: &str| {
        records
            .iter()
            .find(|r| r.key.request.0 == request)
            .unwrap_or_else(|| panic!("{request}: {records:?}"))
    };
    let tools = |chain: &bot_core::pass::PassChain| -> Vec<String> {
        chain
            .candidates
            .iter()
            .map(|c: &Candidate| c.trigger.clone())
            .collect()
    };
    let first = by_request("Ev-R1").chain.clone().unwrap();
    assert_eq!(tools(&first), ["first", "second"]);
    // Reaction default fallback carries only validated reaction metadata in fixed JSON.
    assert!(
        matches!(
            &first.default,
            Some(bot_core::command::Target::Agent { agent, instruction })
                if agent.0 == "main"
                    && instruction == r#"{"event":"reaction_added","item_ts":"100.1","reaction":"white_check_mark"}"#
        ),
        "{:?}",
        first.default
    );
    let second = by_request("Ev-R2").chain.clone().unwrap();
    assert_eq!(tools(&second), ["second"]);
    assert!(matches!(
        &second.default,
        Some(bot_core::command::Target::Agent { instruction, .. })
            if instruction == r#"{"event":"reaction_added","item_ts":"100.2","reaction":"eyes"}"#
    ));
    let mention = by_request("C1.200.1").chain.clone().unwrap();
    assert_eq!(tools(&mention), ["answer"]);
    assert!(matches!(
        &mention.default,
        Some(bot_core::command::Target::Agent { instruction, .. }) if instruction == "question"
    ));
}

#[tokio::test]
async fn a_member_without_the_run_scope_gets_the_tool_candidates_but_no_default_conversation() {
    let rig = connected(vec![
        channel_profile(&ALL)
            .trigger("main", reaction_trigger("approve", 0, "agenda.list"))
            .trigger("main", {
                let mut t = event_trigger("answer", "team", tool_target("agenda.list", "{}"));
                t.order = 0;
                t
            }),
    ])
    .await;
    push_all(
        &rig,
        vec![
            reaction_frame("R1", TC1, "U5", "100.1", "white_check_mark", "U9"),
            reaction_frame("R2", TC1, "U1", "100.2", "white_check_mark", "U9"),
            mention_frame("E1", "T1", "C1", "U5", "200.1", "<@UBOT> question"),
            mention_frame("E2", "T1", "C1", "U1", "200.2", "<@UBOT> question"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    let chain_of = |request: &str| {
        records
            .iter()
            .find(|r| r.key.request.0 == request)
            .unwrap_or_else(|| panic!("{request}: {records:?}"))
            .chain
            .clone()
            .unwrap()
    };
    for (request, has_default) in [
        ("Ev-R1", false),
        ("Ev-R2", true),
        ("C1.200.1", false),
        ("C1.200.2", true),
    ] {
        let chain = chain_of(request);
        assert_eq!(chain.candidates.len(), 1, "{request}: candidates preserved");
        assert_eq!(chain.default.is_some(), has_default, "{request}");
    }
}

#[tokio::test]
async fn a_reaction_from_a_bot_slackbot_or_an_unverifiable_account_is_acked_but_never_accepted() {
    let rig = connected(vec![
        channel_profile(&[SlackEvent::Mention, SlackEvent::Reaction])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    let react = |n: u32, user: &str| {
        reaction_frame(
            &format!("R{n}"),
            TC1,
            user,
            &format!("100.{n}"),
            "white_check_mark",
            "U9",
        )
    };
    let lookups = || rig.slack.calls("users.info");

    rig.slack.set_user("U3", json!({"is_bot": true}));
    push_all(&rig, vec![react(1, "U3")]).await;
    rig.slack.set_user("U3", json!({"deleted": true}));
    push_all(&rig, vec![react(2, "U3")]).await;
    rig.slack.set_user("U3", json!({}));
    // USLACKBOT reports `is_bot=false` in Slack APIs, so it is rejected locally without lookup.
    let before = lookups().len();
    push_all(&rig, vec![react(3, "USLACKBOT")]).await;
    assert_eq!(
        lookups().len(),
        before,
        "Slackbot must be rejected without calling Slack"
    );

    let human = |id: &str| json!({"ok": true, "user": {"id": id, "is_bot": false}});
    let failures = [
        Scripted::Json(json!({"ok": false, "error": "missing_scope", "needed": "users:read"})),
        Scripted::Raw {
            status: 429,
            retry_after: Some(1),
            body: "{}".into(),
        },
        Scripted::Raw {
            status: 500,
            retry_after: None,
            body: "oops".into(),
        },
        Scripted::Json(json!({"ok": false, "error": "invalid_auth"})),
        Scripted::Json(human("U2")),
        Scripted::Json(json!({"ok": true, "user": {"id": "U3"}})),
        // `deleted: null` is ambiguous and fails closed.
        Scripted::Json(json!({"ok": true, "user": {"id": "U3", "is_bot": false, "deleted": null}})),
        Scripted::Delayed {
            delay: std::time::Duration::from_secs(3),
            body: human("U3"),
        },
    ];
    let count = failures.len() as u32;
    for (n, failure) in (10..).zip(failures) {
        let stalls = matches!(failure, Scripted::Delayed { .. });
        rig.slack.script("users.info", failure);
        let started = std::time::Instant::now();
        push_all(&rig, vec![react(n, "U3")]).await;
        if stalls {
            // Stalled lookups must abort within Slack's 3s ack deadline.
            assert!(
                started.elapsed() < std::time::Duration::from_millis(2900),
                "{:?}",
                started.elapsed()
            );
        }
    }
    assert!(
        rig.store.accepted().is_empty(),
        "non-human or unverifiable accounts must not be accepted: {:?}",
        rig.store.accepted()
    );
    assert_eq!(rig.slack.acks().len() as u32, 3 + count);
    // Timed-out lookup aborts client-side before the fake server records a response.
    let expected = (1 + count) as usize;
    assert_eq!(lookups().len(), expected);
    assert!(
        lookups()
            .iter()
            .all(|call| call.body["user"] == "U3" && !call.token.is_empty()),
        "lookups must use the installation bot token for the queried user only"
    );

    push_all(&rig, vec![react(30, "U3")]).await;
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn a_human_who_is_deactivated_between_two_reactions_is_not_accepted_the_second_time() {
    let rig = connected(vec![
        channel_profile(&[SlackEvent::Mention, SlackEvent::Reaction])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    let react = |n: u32| {
        reaction_frame(
            &format!("R{n}"),
            TC1,
            "U1",
            &format!("100.{n}"),
            "white_check_mark",
            "U9",
        )
    };
    push_all(&rig, vec![react(1)]).await;
    assert_eq!(rig.store.accepted().len(), 1);
    rig.slack.set_user("U1", json!({"deleted": true}));
    push_all(&rig, vec![react(2)]).await;
    assert_eq!(
        rig.store.accepted().len(),
        1,
        "reaction after deactivation must not be accepted"
    );
    rig.slack.set_user("U1", json!({}));
    push_all(&rig, vec![react(3)]).await;
    assert_eq!(rig.store.accepted().len(), 2);
    assert_eq!(
        rig.slack.calls("users.info").len(),
        3,
        "human status must not be cached across reactions"
    );
    // Non-members are rejected before querying `users.info`.
    let before = rig.slack.calls("users.info").len();
    push_all(
        &rig,
        vec![reaction_frame(
            "R9",
            TC1,
            "U9",
            "100.9",
            "white_check_mark",
            "U1",
        )],
    )
    .await;
    assert_eq!(rig.slack.calls("users.info").len(), before);
}

#[tokio::test]
async fn the_reply_thread_of_a_reaction_is_only_a_thread_the_bot_took_part_in() {
    let rig = connected(vec![
        channel_profile(&[SlackEvent::Mention, SlackEvent::Reaction])
            .trigger("main", reaction_trigger("approve", 0, "agenda.list")),
    ])
    .await;
    rig.threads.seed(&thread("200.1"));
    push_all(
        &rig,
        vec![
            reaction_frame("R1", TC1, "U1", "200.1", "white_check_mark", "U9"),
            // Reply inside the thread (not the root) and unproven posts both stay top-level.
            reaction_frame("R2", TC1, "U1", "200.5", "white_check_mark", "U9"),
            reaction_frame("R3", TC1, "U1", "300.1", "white_check_mark", "U9"),
        ],
    )
    .await;
    let records = rig.store.accepted();
    let conversation = |request: &str| {
        records
            .iter()
            .find(|r| r.key.request.0 == request)
            .unwrap_or_else(|| panic!("{request}: {records:?}"))
            .conversation
            .clone()
    };
    assert_eq!(conversation("Ev-R1"), "channel:C1:200.1");
    assert_eq!(conversation("Ev-R2"), "channel:C1:");
    assert_eq!(conversation("Ev-R3"), "channel:C1:");
}
