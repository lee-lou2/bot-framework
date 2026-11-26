//! 넘김 사슬과 지정한 사람의 승인 증거 이음매. 저장소(`bot-store`의 `evidence::bind_in`)는 인증된 원천(`extras.source`)과 후보 사슬(`extras.chain`)이 **둘 다** 있고
//! 첫 후보가 접수의 대상과 같은 직접 도구일 때만 지정한 사람의 승인 증거를 묶는다. 하나라도 어긋나면 조용히 만들지 않고 카드 규칙으로 돌아가므로,
//! 게이트웨이가 직접 도구 Slack 이벤트(반응·멘션)마다 그 조건을 채우는지 실제 수신 경로로 고정한다. 저장소 쪽(접수 커밋 안의 증거 묶기·소비)은
//! `bot-store/tests/human_approval.rs`가 같은 모양의 `AcceptExtras`로 실제 PG 위에서 확인한다.
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
mod support;
use support::*;

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
        other => panic!("직접 도구가 아니다: {other:?}"),
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
            "<@UBOT> 묻는다",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() >= want).await);

    let records = rig.store.accepted();
    let mut kinds: Vec<(SourceKind, String)> = records
        .iter()
        .map(|r| {
            let source = r
                .source
                .as_ref()
                .unwrap_or_else(|| panic!("직접 도구 접수에 인증된 원천이 없다: {r:?}"));
            let chain: &PassChain = r
                .chain
                .as_ref()
                .unwrap_or_else(|| panic!("직접 도구 접수에 후보 사슬이 없다: {r:?}"));
            let first = chain.candidates.first().expect("사슬의 첫 후보");
            assert_eq!(
                tool_text(&first.target),
                r.target,
                "첫 후보는 접수의 대상과 같아야 저장소가 증거를 묶는다"
            );
            assert!(
                r.key.principal.0.starts_with("slack:"),
                "행위자는 Slack 사람 주체다: {:?}",
                r.key.principal
            );
            assert_eq!(source.team, "T1");
            (source.kind, first.trigger.clone())
        })
        .collect();
    // 반응은 접수 앞에서 반응한 사람을 확인하느라(`users.info`) 멘션보다 늦게 접수될 수 있다: 순서와 상관없이 종류별 첫 후보를 본다
    kinds.sort_by_key(|(kind, _)| format!("{kind:?}"));
    assert_eq!(
        kinds,
        [
            (SourceKind::Message, "answer".to_owned()),
            (SourceKind::Reaction, "confirm".to_owned())
        ],
        "반응은 confirm, 멘션은 answer가 첫 후보다: {records:?}"
    );
}
