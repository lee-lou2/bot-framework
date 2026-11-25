//! F2 반응 수신과 F3 넘김 후보 사슬의 구성: 반응한 사람은 지금도 활성 «사람» 계정이어야 하고(`users.info`), 회신은 봇이 참여한 것으로 확인된 스레드로만 가며,
//! 후보가 모두 넘기면 기본 에이전트가 고정 형식의 사건 입력을 받는다. 가짜 Slack에 소켓으로 붙어 인증된 설치 → 접수 → ack를 확인한다.
use bot_core::{
    pass::Candidate,
    slack::{SlackEvent, SourceKind},
};
use bot_gateway::testing::{
    slack::{Scripted, mention_frame, reaction_frame},
    *,
};
use serde_json::{Value, json};
mod support;
use support::{events::*, *};

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
            // 행위자 U1, 원글 작성자 U9(다른 사람)
            reaction_frame("R1", TC1, "U1", "100.1", "white_check_mark", "U9"),
            // 같은 이벤트의 재전달
            reaction_frame("R1", TC1, "U1", "100.1", "white_check_mark", "U9"),
            // 조건에 맞지 않는 반응·등록 안 된 행위자·봇 자신의 반응·다른 채널·다른 팀·DM
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
        "요청자는 반응한 사람이다(원글 작성자가 아니다)"
    );
    assert_eq!(r.key.request.0, "Ev-R1");
    assert_eq!(
        r.conversation, "channel:C1:",
        "봇이 참여한 것으로 확인되지 않은 글이면 스레드로 짐작하지 않고 채널 최상위다"
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

/// 봇 자신의 반응은 멤버 확인에 기대지 않고 막는다: 봇 사용자가 (잘못) 멤버로 등록돼 있어도 접수하지 않는다.
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
            // 대조군: 같은 반응을 사람이 달면 접수된다
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
    // `reaction`을 켜지 않은 바인딩
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
    // 켰지만 이 사건을 받는 트리거가 없다(후보가 없으면 사슬도 기본 대화도 없다)
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
            // 조건이 맞지 않으면 첫 후보는 `second`다
            reaction_frame("R2", TC1, "U1", "100.2", "eyes", "U9"),
            mention_frame("E1", "T1", "C1", "U1", "200.1", "<@UBOT> 묻는다"),
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
    // 반응의 기본 대화: 원래 바인딩의 에이전트가 검증한 반응 이름과 글 ts만 담은 고정 형식 입력을 받는다(원글 본문·작성자·자유 문장 없음)
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
    // 멘션: 후보 하나(`answer`)와 글 그대로의 기본 대화
    let mention = by_request("C1.200.1").chain.clone().unwrap();
    assert_eq!(tools(&mention), ["answer"]);
    assert!(matches!(
        &mention.default,
        Some(bot_core::command::Target::Agent { instruction, .. }) if instruction == "묻는다"
    ));
}

/// `run` 스코프가 없는 멤버(도구만 쓸 수 있다)는 기본 대화(에이전트 실행)를 사슬에 받지 못한다: 접수는 첫 후보(도구)만 보고 이 스코프를 요구하지 않는다.
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
            // `run`이 없는 U5와 있는 U1이 같은 반응·같은 멘션을 보낸다
            reaction_frame("R1", TC1, "U5", "100.1", "white_check_mark", "U9"),
            reaction_frame("R2", TC1, "U1", "100.2", "white_check_mark", "U9"),
            mention_frame("E1", "T1", "C1", "U5", "200.1", "<@UBOT> 묻는다"),
            mention_frame("E2", "T1", "C1", "U1", "200.2", "<@UBOT> 묻는다"),
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
        assert_eq!(chain.candidates.len(), 1, "{request}: 후보는 그대로다");
        assert_eq!(chain.default.is_some(), has_default, "{request}");
    }
}

/// 반응한 사람이 지금 활성 «사람» 계정이 아니면 후보·기본 대화·승인 증거 없이 ack만 한다. 성공한 사람 판정은 기억하지 않아 이벤트마다 `users.info`를 부른다.
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

    // 다른 앱의 봇 계정(멤버로 올라 있어도): `is_bot=true`
    rig.slack.set_user("U3", json!({"is_bot": true}));
    push_all(&rig, vec![react(1, "U3")]).await;
    // 비활성화한 계정
    rig.slack.set_user("U3", json!({"deleted": true}));
    push_all(&rig, vec![react(2, "U3")]).await;
    rig.slack.set_user("U3", json!({}));
    // Slackbot은 `is_bot=false`지만 사람이 아니다: 조회 없이 거른다
    let before = lookups().len();
    push_all(&rig, vec![react(3, "USLACKBOT")]).await;
    assert_eq!(
        lookups().len(),
        before,
        "Slackbot은 Slack을 부르지 않고 거른다"
    );

    // 확인하지 못하는 경우: scope 부족·속도 제한·5xx·토큰 거절·다른 사용자의 응답·필드 없음·시간 초과
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
        // `deleted`는 생략할 때만 정상이다: null은 판정할 수 없다
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
            // 응답이 늦으면 ack 기한(3초) 안에 조회를 포기하고 ack한다
            assert!(
                started.elapsed() < std::time::Duration::from_millis(2900),
                "{:?}",
                started.elapsed()
            );
        }
    }
    assert!(
        rig.store.accepted().is_empty(),
        "사람이 아니거나 확인하지 못했으면 접수 0: {:?}",
        rig.store.accepted()
    );
    // 모든 이벤트가 ack됐다(다시 오지 않는다). Slackbot 말고는 이벤트마다 조회를 한 번 했다. 시간 초과한 조회는 클라이언트가 끊어 가짜 서버가 응답을 기록하지 못한다
    assert_eq!(rig.slack.acks().len() as u32, 3 + count);
    let expected = (1 + count) as usize;
    assert_eq!(lookups().len(), expected);
    assert!(
        lookups()
            .iter()
            .all(|call| call.body["user"] == "U3" && !call.token.is_empty()),
        "같은 설치의 봇 토큰으로 물은 사용자만 조회한다"
    );

    // 대조: 활성 사람 계정이면 접수한다(앞의 거절·실패가 이후 판정에 남지 않는다)
    push_all(&rig, vec![react(30, "U3")]).await;
    assert_eq!(rig.store.accepted().len(), 1);
}

/// 사람 판정을 기억하지 않는다: 활성 사람이던 계정이 비활성화되면 바로 다음 반응부터 받지 않는다(옛 판정으로 승인이 가능해지지 않는다).
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
        "비활성화한 뒤의 반응은 접수 0"
    );
    rig.slack.set_user("U1", json!({}));
    push_all(&rig, vec![react(3)]).await;
    assert_eq!(rig.store.accepted().len(), 2);
    assert_eq!(
        rig.slack.calls("users.info").len(),
        3,
        "성공한 판정도 기억하지 않아 반응마다 한 번 묻는다"
    );
    // 멤버가 아닌 사람의 반응은 Slack을 부르지 않는다(멤버 확인이 먼저다)
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

/// 반응의 회신은 봇이 참여한 것으로 확인된 스레드(그 글이 루트)에서만 그 스레드로 간다. 반응이 달린 글의 ts를 루트로 짐작하지 않는다 —
/// 확인된 스레드 안의 답글에 단 반응도, 확인되지 않은 글에 단 반응도 채널 최상위다.
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
            // 같은 스레드 안의 답글(루트가 아니다)에 단 반응
            reaction_frame("R2", TC1, "U1", "200.5", "white_check_mark", "U9"),
            // 봇이 참여하지 않은 글
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
