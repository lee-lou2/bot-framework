//! Slack 입구 수락 시험: 가짜 Slack 서버에 소켓으로 붙어 인증된 설치 → 접수 → ack를 확인한다.
use crate::support::*;
use bot_core::port::Error;
use bot_gateway::{
    slack::{
        frame::{Frame, Message},
        socket::Installation,
    },
    testing::{
        slack::{dm_frame, mention_frame},
        *,
    },
};
use serde_json::json;
#[tokio::test]
async fn a_channel_mention_is_accepted_and_acked_after_the_commit() {
    let rig = connected(vec![personal()]).await;
    rig.slack
        .push(mention_frame(
            "E1",
            "T1",
            "C1",
            "U1",
            "1700000001.000100",
            "<@UBOT> 오늘 일정 알려줘",
        ))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_string()))
            .await
    );
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, "agent main 오늘 일정 알려줘");
    assert_eq!(records[0].key.principal.0, "slack:home/U1");
    assert_eq!(records[0].key.request.0, "C1.1700000001.000100");
    assert_eq!(records[0].conversation, "channel:C1:1700000001.000100");
    assert_eq!(records[0].approver.as_deref(), Some("slack:home/U1"));
    // 소켓을 연 앱 토큰으로 받았고, 봇 사용자 ID는 auth.test로 알았다
    assert_eq!(
        rig.slack.calls("apps.connections.open")[0].token,
        "test-app-1"
    );
    assert_eq!(rig.slack.calls("auth.test")[0].token, "test-bot-1");
}

#[tokio::test]
async fn a_dm_threads_under_the_message_and_redelivery_is_one_request() {
    let rig = connected(vec![personal()]).await;
    rig.slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            "1700000002.000200",
            "질문",
        ))
        .await;
    rig.slack
        .push(dm_frame(
            "E1b",
            "T1",
            "D1",
            "U1",
            "1700000002.000200",
            "질문",
        ))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 2).await);
    let records = rig.store.accepted();
    assert_eq!(
        records.len(),
        1,
        "같은 글은 어떤 이벤트·재전달로 와도 한 요청"
    );
    assert_eq!(records[0].conversation, "direct:D1:1700000002.000200");
    assert_eq!(records[0].target, "agent main 질문");
    // 인증된 이벤트 원천이 같이 간다(넘김·승인 증거의 기준): 두 번째 전달의 event_id가 아니라 처음 접수된 것이다
    let source = records[0].source.as_ref().expect("DM 글의 원천");
    assert_eq!(
        (
            source.kind,
            source.channel.as_str(),
            source.ts.as_str(),
            source.event_id.as_str()
        ),
        (
            bot_core::slack::SourceKind::Message,
            "D1",
            "1700000002.000200",
            "Ev-E1"
        )
    );
}

#[tokio::test]
async fn foreign_unregistered_and_bot_events_are_acked_but_never_accepted() {
    let rig = connected(vec![personal()]).await;
    let bot = json!({"type": "events_api", "envelope_id": "Ebot", "payload": {"team_id": "T1", "event": {
        "type": "message", "channel_type": "im", "user": "U1", "bot_id": "B1", "text": "x", "ts": "5.5", "channel": "D1"}}});
    for frame in [
        mention_frame("Eu", "T1", "C1", "U9", "1.1", "등록 안 된 사람"),
        mention_frame("Ec", "T1", "C9", "U1", "1.2", "연결 안 된 채널"),
        mention_frame("Et", "T9", "C1", "U1", "1.3", "다른 워크스페이스"),
        mention_frame("Ex", "T1", "C1", "U1/x", "1.4", "주체 모양이 이상함"),
        bot,
    ] {
        rig.slack.push(frame).await;
    }
    assert!(rig.slack.until(|s| s.acks().len() == 5).await);
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn an_outage_or_a_rate_limit_is_not_acked_so_slack_redelivers() {
    let rig = connected(vec![personal()]).await;
    rig.store.fail_next(Error::Unavailable);
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "첫 시도"))
        .await;
    settle().await;
    assert!(rig.slack.acks().is_empty(), "저장소 장애는 ack하지 않는다");
    // Slack이 같은 글을 다시 보내면 접수되고 ack된다
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "첫 시도"))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    assert_eq!(rig.store.accepted().len(), 1);

    // 속도 제한: 주체 한도 1건
    let limited = rig_limited().await;
    limited
        .slack
        .push(mention_frame("L1", "T1", "C1", "U1", "2.1", "하나"))
        .await;
    assert!(limited.slack.until(|s| s.acks().len() == 1).await);
    limited
        .slack
        .push(mention_frame("L2", "T1", "C1", "U1", "2.2", "둘"))
        .await;
    settle().await;
    assert_eq!(
        limited.slack.acks(),
        ["L1"],
        "일시적 속도 제한은 ack하지 않는다"
    );
    assert_eq!(limited.store.accepted().len(), 1);
}

async fn rig_limited() -> Rig {
    let mut config = host("");
    config.limits.per_principal_per_min = Some(1);
    let rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    assert!(rig.slack.connected().await);
    rig
}

#[tokio::test]
async fn a_private_dm_reaches_only_the_private_intake() {
    let marker = "합성-민감-표식-91c2";
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    let rig = connected(vec![personal().trigger("main", secure)]).await;
    rig.slack
        .push(dm_frame("E1", "T1", "D1", "U1", "3.3", marker))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    assert!(rig.store.accepted().is_empty());
    assert_eq!(
        rig.private.received(),
        [(
            "inbox.receive".to_string(),
            format!(r#"{{"text":"{marker}"}}"#)
        )]
    );
    // 전용 입구가 실패하면 ack하지 않고 일반 대화로도 넘기지 않는다
    rig.private.fail_next(Error::Unavailable);
    rig.slack
        .push(dm_frame("E2", "T1", "D1", "U1", "3.4", marker))
        .await;
    settle().await;
    assert_eq!(rig.slack.acks(), ["E1"]);
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn a_hello_from_another_app_stops_the_socket_and_nothing_is_processed() {
    let rig = rig(vec![personal()], default_secrets(), |s| {
        s.set_hello_app(Some("AOTHER"))
    })
    .await;
    assert!(rig.slack.connected().await);
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "x"))
        .await;
    settle().await;
    assert!(rig.slack.acks().is_empty());
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn reconnects_after_the_server_drops_the_connection() {
    let rig = connected(vec![personal()]).await;
    rig.slack.drop_connection();
    assert!(
        rig.slack.until(|s| s.connects() == 2).await,
        "재접속하지 않았어요"
    );
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "다시 붙은 뒤"))
        .await;
    assert!(
        rig.slack
            .until(|s| s.acks().contains(&"E1".to_string()))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 1);
}

#[tokio::test]
async fn startup_refuses_shared_tokens_foreign_workspaces_and_missing_credentials() {
    // 같은 앱 토큰을 프로필 둘이 쓰면 이벤트가 나뉘어 한쪽이 놓친다
    let shared = secrets(&[
        ("personal", "bot-token", "test-bot-1"),
        ("personal", "app-token", "same-app-token"),
        ("work", "bot-token", "test-bot-2"),
        ("work", "app-token", "same-app-token"),
    ]);
    let mut rig = rig(vec![personal(), work()], shared, |_| {}).await;
    let error = startup_failure(&mut rig).await;
    assert!(error.0.contains("같은 앱 토큰"), "{error}");
    assert!(!error.0.contains("same-app-token"), "토큰 값은 싣지 않는다");

    // 토큰이 설정한 워크스페이스의 것이 아니면 거절
    let mut rig = self::rig(vec![personal()], default_secrets(), |s| {
        s.set_auth("T9", "UBOT")
    })
    .await;
    let error = startup_failure(&mut rig).await;
    assert!(error.0.contains("설정한 워크스페이스"), "{error}");

    // 자격이 없으면 거절하고 소켓을 열지 않는다
    let mut rig = self::rig(
        vec![personal()],
        secrets(&[("personal", "bot-token", "test-bot-1")]),
        |_| {},
    )
    .await;
    let slack_calls = rig.slack.all_calls().len();
    let error = startup_failure(&mut rig).await;
    assert!(error.0.contains("토큰을 읽지 못했어요"), "{error}");
    assert_eq!(slack_calls, 0);
}

#[tokio::test]
async fn each_profile_gets_its_own_socket_and_routes_only_its_own_bindings() {
    let both = secrets(&[
        ("personal", "bot-token", "test-bot-1"),
        ("personal", "app-token", "test-app-1"),
        ("work", "bot-token", "test-bot-2"),
        ("work", "app-token", "test-app-2"),
    ]);
    let rig = rig(vec![personal(), work()], both, |_| {}).await;
    assert!(
        rig.slack.until(|s| s.connects() == 2).await,
        "프로필마다 소켓 하나"
    );
    let plan = rig.gateway.plan().await.unwrap();
    assert_eq!(plan.len(), 2);
    let install = |profile: &str| -> Installation {
        plan.iter()
            .find(|i| i.profile.0 == profile)
            .unwrap()
            .clone()
    };
    let message = Message {
        team: "T1".into(),
        channel: "C1".into(),
        direct: false,
        user: "U1".into(),
        text: "<@UBOT> hi".into(),
        ts: "9.9".into(),
        thread_ts: None,
        files: vec!["F1".into()],
        event_id: None,
    };
    // work 설치로 온 personal 채널(C1)의 글은 받았다는 표시만 하고 접수하지 않는다
    assert!(
        rig.gateway
            .on_frame(&install("work"), Frame::Message(message.clone()))
            .await
    );
    assert!(rig.store.accepted().is_empty());
    // 자기 설치로 오면 접수된다
    assert!(
        rig.gateway
            .on_frame(&install("personal"), Frame::Message(message))
            .await
    );
    assert_eq!(rig.store.accepted().len(), 1);
    assert_eq!(rig.store.accepted()[0].key.profile.0, "personal");
}

#[tokio::test]
async fn a_message_is_admitted_under_the_table_that_proved_its_owner() {
    use bot_core::profile::Adapter;
    let rig = connected(vec![personal()]).await;
    // 소유자 확인 직후(첫 표 조회 뒤) 같은 채널의 소유자가 다른 프로필로 바뀐다
    let work_owns_planning = profile("work")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .member("slack:home/U1", &["owner"])
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .build();
    rig.catalog.flip_after(1, [work_owns_planning]);
    rig.slack
        .push(mention_frame("E1", "T1", "C1", "U1", "1.1", "hi"))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 1).await);
    let records = rig.store.accepted();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].key.profile.0, "personal",
        "소유자를 확인한 설치의 프로필로만 접수된다(새 소유자 프로필로 새면 안 돼요)"
    );
    // 표가 바뀐 뒤 같은 설치로 온 글은 이 설치의 프로필 바인딩이 아니라서 받았다는 표시만 한다
    rig.slack
        .push(mention_frame("E2", "T1", "C1", "U1", "1.2", "again"))
        .await;
    assert!(rig.slack.until(|s| s.acks().len() == 2).await);
    assert_eq!(rig.store.accepted().len(), 1);
}

/// 소켓이 아닌 수신 방식은 받아 줄 곳이 없다. 설정 검증이 먼저 거르지만(`bot validate`·시작), 검증을 거치지 않은 설정이 들어와도 Slack 게이트웨이는
/// 그 설치를 **조용히 소켓으로 읽지 않고** 검증과 같은 사유(워크스페이스 별칭·필드 경로)로 시작을 거절한다. 자격을 읽거나 Slack을 부르기 전이다.
#[tokio::test]
async fn an_unvalidated_http_receive_declaration_stops_the_gateway_instead_of_being_read_as_socket()
{
    let declared = |receive: Option<&str>| {
        let mut home = json!({"team_id": "T1", "app_id": "A1", "dm": true,
            "bot_token": "bot-token", "app_token": "app-token", "signing_secret": "signing",
            "channels": {"planning": "C1"}});
        if let Some(receive) = receive {
            home["receive"] = json!(receive);
        }
        json!({"slack": {"workspaces": {"home": home}}})
    };
    // 검증을 거치지 않은 설정(`HostConfig`는 `Deserialize`로 그대로 만들 수 있다)
    let config: bot_gateway::host::HostConfig =
        serde_json::from_value(declared(Some("http"))).unwrap();
    assert!(config.validate().is_err(), "설정 검증이 먼저 거른다");
    let mut rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    let error = startup_failure(&mut rig).await;
    assert!(
        error
            .0
            .starts_with("gateway.slack.workspaces.home.receive: "),
        "{error}"
    );
    assert!(
        rig.slack.all_calls().is_empty(),
        "Slack에 아무것도 부르지 않았어요"
    );
    // 선언만 빼면 같은 설정이 검증을 통과해 받는다
    let config = bot_gateway::host::HostConfig::from_value(declared(None)).unwrap();
    let rig = rig_with(vec![personal()], default_secrets(), config, |_| {}).await;
    assert!(rig.slack.connected().await);
}
