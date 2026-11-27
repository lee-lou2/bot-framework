//! 예약 회신의 명시 목적지(정규 경로 `팀ID/채널ID`) 발송 시험: Slack API에는 채널 ID만 가고, 발송 직전에 지금 호스트 등록부의 같은 팀·채널일 때만 보낸다.

use crate::delivery_support::*;
use crate::support::{default_secrets, personal};
use bot_gateway::{
    host::HostConfig,
    ingress::Ingress,
    slack::{SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, outgoing},
        slack::FakeSlack,
        *,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

fn pinned(rig: &Rig, seq: u32, id: &str, body: Value) {
    rig.outbox.push(outgoing(
        "personal",
        seq,
        "team",
        "slack:home/U1",
        channel(id),
        &body.to_string(),
    ));
}

#[tokio::test]
async fn a_pinned_route_is_sent_by_channel_id_only_and_only_for_the_registered_team_and_channel() {
    let rig = rig().await;
    pinned(&rig, 1, "T1/C1", json!({"text": "고정된 경로"}));
    assert_eq!(rig.deliverer.tick().await.unwrap(), 1);
    // Slack에는 채널 ID만 간다(팀은 보내기 전에 대조했다). 영수증도 채널 ID로 남는다
    let calls = posts(&rig);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["channel"], "C1");
    assert_eq!(calls[0].body["thread_ts"], THREAD);
    assert_eq!(settled(&rig), ["confirmed:C1:1700000000.000002"]);

    // 다른 팀의 같은 채널 ID·이 바인딩의 채널이 아닌 곳·모양이 틀린 경로·팀 접두사 소문자는 보내지 않는다
    for (seq, id) in [
        (2, "T9/C1"),
        (3, "T1/C2"),
        (4, "T1/C9"),
        (5, "T1/C1/C1"),
        (6, "T1/"),
        (7, "t1/C1"),
    ] {
        pinned(&rig, seq, id, json!({"text": "보내면 안 돼요"}));
    }
    assert_eq!(rig.deliverer.tick().await.unwrap(), 6);
    assert_eq!(posts(&rig).len(), 1, "Slack에는 처음 한 건만 갔어요");
    assert_eq!(settled(&rig)[1..], ["rejected"; 6]);
}

/// `personal()`의 바인딩(채널 `planning`·DM)이 그대로 성립하는 등록부 하나
fn registry(team: &str, planning: &str) -> String {
    format!(
        r#"{{"slack": {{"workspaces": {{"home": {{"team_id": "{team}", "dm": true, "bot_token": "bot-token",
            "app_token": "app-token", "channels": {{"planning": "{planning}", "ops": "C2"}}}}}}}}}}"#
    )
}

#[tokio::test]
async fn a_pinned_route_is_checked_against_the_registry_at_send_time() {
    // 등록부가 그 사이에 바뀌었다: 팀이 다르다 / 별칭이 다른 채널을 가리킨다
    for host in [registry("T2", "C1"), registry("T1", "C7")] {
        let rig = rig_on(&host, personal(), default_secrets(), None).await;
        pinned(&rig, 1, "T1/C1", json!({"text": "옛 경로"}));
        assert_eq!(rig.deliverer.tick().await.unwrap(), 1, "{host}");
        assert!(rig.slack.all_calls().is_empty(), "보내지 않았어요: {host}");
        assert_eq!(settled(&rig), ["rejected"], "{host}");
    }
    // 새 등록부의 경로는 그대로 간다(채널 ID만)
    let rig = rig_on(&registry("T1", "C7"), personal(), default_secrets(), None).await;
    pinned(&rig, 1, "T1/C7", json!({"text": "새 경로"}));
    rig.deliverer.tick().await.unwrap();
    assert_eq!(posts(&rig)[0].body["channel"], "C7");
    // 바인딩이 프로필에서 빠졌어도(같은 경로 문자열) 보내지 않는다
    let rig = self::rig().await;
    rig.outbox.push(outgoing(
        "personal",
        1,
        "gone",
        "slack:home/U1",
        channel("T1/C1"),
        &json!({"text": "빠진 바인딩"}).to_string(),
    ));
    rig.deliverer.tick().await.unwrap();
    assert!(rig.slack.all_calls().is_empty());
    assert_eq!(settled(&rig), ["rejected"]);
}

#[tokio::test]
async fn a_registry_that_no_longer_matches_the_profile_leaves_the_item_alone() {
    // 프로필의 바인딩이 가리키는 채널·워크스페이스가 등록부에서 사라지면 입구 표가 서지 않는다: 보내지도 정산하지도 않는다
    for host in [
        r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token", "channels": {"ops": "C2"}}}}}"#,
        r#"{"slack": {"workspaces": {}}}"#,
    ] {
        let rig = rig_on(host, personal(), default_secrets(), None).await;
        pinned(&rig, 1, "T1/C1", json!({"text": "빠진 수신자"}));
        assert_eq!(rig.deliverer.tick().await.unwrap(), 1, "{host}");
        assert!(rig.slack.all_calls().is_empty(), "{host}");
        assert!(
            settled(&rig).is_empty() && rig.outbox.begun().is_empty(),
            "{host}"
        );
    }
}

/// Deliverer를 거치지 않고 `send`를 직접 부르는 보내기. 등록부를 `host`로 바꿔 끼운다.
fn direct_sender(host: &str, slack: &FakeSlack) -> bot_gateway::slack::send::SlackSender {
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new([personal().build()]),
        Arc::new(HostConfig::from_json(host).unwrap()),
        MemoryStore::new(),
        MemoryPrivate::new(),
    ));
    SlackGateway::new(
        ingress,
        MemoryBroker::new(),
        MemoryLedger::new(),
        default_secrets(),
        SlackApi::new(Some(&slack.base)),
    )
    .sender()
}

#[tokio::test]
async fn the_sender_itself_refuses_a_route_the_registry_does_not_hold_even_when_called_directly() {
    // `send`가 마지막 문이다: 앞 단계(Deliverer의 `allows`)를 믿지 않는다
    use bot_gateway::{
        delivery::{Delivery, Sender},
        reply::ReplyBody,
    };
    let body = ReplyBody::parse(&bot_core::command::JsonPayload(
        json!({"text": "직접"}).to_string(),
    ))
    .unwrap();
    let snapshot = personal().build();
    let item =
        |seq, id: &str| outgoing("personal", seq, "team", "slack:home/U1", channel(id), "{}");

    // 등록부가 맞는 호스트: 팀·채널이 어긋난 경로는 거절, 맞는 경로는 채널 ID로 한 번
    let slack = FakeSlack::start().await;
    let sender = direct_sender(HOST, &slack);
    for (seq, id) in [(1, "T9/C1"), (2, "T1/C2"), (3, "T1/C1/C1"), (4, "C2")] {
        let got = sender.send(&snapshot, &item(seq, id), &body).await;
        assert_eq!(got, Delivery::Rejected, "{id}");
    }
    assert!(slack.all_calls().is_empty(), "Slack을 부르지 않았어요");
    let got = sender.send(&snapshot, &item(5, "T1/C1"), &body).await;
    assert!(matches!(got, Delivery::Sent { .. }), "{got:?}");
    let sent = slack.calls("chat.postMessage");
    assert_eq!(
        (sent.len(), sent[0].body["channel"].as_str()),
        (1, Some("C1"))
    );

    // 등록부가 그 채널·워크스페이스를 잃었으면 원래 대화 모양(`C1`)도 정규 경로도 보내지 않는다
    for host in [
        registry_without_channel(),
        r#"{"slack": {"workspaces": {}}}"#.to_owned(),
    ] {
        let slack = FakeSlack::start().await;
        let sender = direct_sender(&host, &slack);
        for (seq, id) in [(1, "C1"), (2, "T1/C1")] {
            let got = sender.send(&snapshot, &item(seq, id), &body).await;
            assert_eq!(got, Delivery::Rejected, "{id} {host}");
        }
        assert!(slack.all_calls().is_empty(), "{host}");
    }
}

fn registry_without_channel() -> String {
    r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token", "channels": {"ops": "C2"}}}}}"#.to_owned()
}

#[tokio::test]
async fn a_pinned_route_with_attachments_uploads_to_the_channel_id() {
    let rig = rig().await;
    pinned(
        &rig,
        1,
        "T1/C1",
        json!({"text": "보고서예요", "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    pinned(
        &rig,
        2,
        "T9/C1",
        json!({"text": "다른 팀", "attachments": [{"name": "b.txt", "type": "text/plain", "data": "aGk="}]}),
    );
    rig.deliverer.tick().await.unwrap();
    let done = rig.slack.calls("files.completeUploadExternal");
    assert_eq!(done.len(), 1, "다른 팀 건은 업로드도 시작하지 않아요");
    assert_eq!(done[0].body["channel_id"], "C1");
    assert_eq!(rig.slack.calls("files.getUploadURLExternal").len(), 1);
    assert_eq!(settled(&rig)[1], "rejected");
}
