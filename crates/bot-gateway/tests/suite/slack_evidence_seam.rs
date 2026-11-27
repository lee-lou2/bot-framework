//! Pass chain and designated-human approval evidence seam: verifies direct-tool Slack events carry both `extras.source` and `extras.chain` with the accepted target first.
use crate::support::*;
use bot_core::{
    command::{JsonPayload, Target},
    pass::PassChain,
    profile::{Adapter, Trigger},
    slack::{SlackEvent, SourceKind},
};
use bot_gateway::testing::{
    slack::{mention_frame, reaction_frame},
    *,
};

const TC1: (&str, &str) = ("T1", "C1");

fn reaction_trigger(id: &str, tool: &str) -> Trigger {
    let mut trigger = event_trigger_on(id, "team", "reaction", tool_target(tool, "{}"));
    trigger.conditions.insert(
        "body.reaction".into(),
        JsonPayload("\"white_check_mark\"".into()),
    );
    trigger
}

fn tool_text(target: &Target) -> String {
    match target {
        Target::Tool { agent, tool, args } => format!("tool {} {} {}", agent.0, tool.0, args.0),
        other => panic!("not a direct tool target: {other:?}"),
    }
}

#[tokio::test]
async fn direct_tool_events_carry_the_authenticated_source_and_a_chain_whose_first_candidate_is_the_accepted_target()
 {
    let rig = connected(vec![
        profile("personal")
            .binding(
                "team",
                Adapter::SlackChannel {
                    workspace: "home".into(),
                    channel: "planning".into(),
                    events: [SlackEvent::Mention, SlackEvent::Reaction]
                        .into_iter()
                        .collect(),
                },
                "main",
            )
            .member("slack:home/U1", &["owner"])
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .trigger("main", reaction_trigger("confirm", "agenda.confirm"))
            .trigger(
                "main",
                event_trigger("answer", "team", tool_target("agenda.list", "{}")),
            ),
    ])
    .await;
    let want = rig.slack.acks().len() + 2;
    rig.slack
        .push(reaction_frame(
            "R1",
            TC1,
            "U1",
            "100.1",
            "white_check_mark",
            "U9",
        ))
        .await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "200.1",
            "<@UBOT> ask",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() >= want).await);

    let records = rig.store.accepted();
    let mut kinds: Vec<(SourceKind, String)> = records
        .iter()
        .map(|r| {
            let source = r.source.as_ref().unwrap_or_else(|| {
                panic!("direct tool admission missing authenticated source: {r:?}")
            });
            let chain: &PassChain = r
                .chain
                .as_ref()
                .unwrap_or_else(|| panic!("direct tool admission missing candidate chain: {r:?}"));
            let first = chain.candidates.first().expect("first chain candidate");
            assert_eq!(
                tool_text(&first.target),
                r.target,
                "first candidate must match accepted target so the store binds approval evidence"
            );
            assert!(
                r.key.principal.0.starts_with("slack:"),
                "actor must be a Slack principal: {:?}",
                r.key.principal
            );
            assert_eq!(source.team, "T1");
            (source.kind, first.trigger.clone())
        })
        .collect();
    // Reactions call `users.info` before admission and may land after mentions; compare sorted by kind.
    kinds.sort_by_key(|(kind, _)| format!("{kind:?}"));
    assert_eq!(
        kinds,
        [
            (SourceKind::Message, "answer".to_owned()),
            (SourceKind::Reaction, "confirm".to_owned())
        ],
        "reaction -> confirm, mention -> answer: {records:?}"
    );
}
