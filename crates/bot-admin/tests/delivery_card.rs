//! 예약 전송 카드(`schedule.deliver`)의 «전송 초안» 행은 예약을 만든 본인(요청자이자 승인자)이 인증된 경우에만 나가고,
//! 승인은 카드에 보이는 초안 원문의 SHA-256이 승인 지문(`args_sha256`)과 같을 때만 받는다. 운영자도 예외가 아니다.
//! 카드는 실제 코어의 `DeliveryDraft`가 만든 문서와 지문으로 꾸며 계약이 어긋나면 여기서 깨진다.

mod support;
use bot_admin::{
    fake::{Fake, approval_view, pid},
    json::hex,
    port::ApprovalView,
};
use bot_core::{
    command::JsonPayload,
    id::{BindingId, CallId, Revision, ToolId},
    schedule::{
        DELIVER_CALL, DELIVER_REVISION, DELIVER_TOOL, DELIVERY_CARD_LABEL, DeliveryDraft,
        DeliveryTarget,
    },
    state::{Conversation, ConversationKind, Risk},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use support::*;

/// 카드를 어긋나게 고치는 함수.
type Edit = Box<dyn FnOnce(&mut ApprovalView)>;

const SECRET: &str = "PRIVATE-DRAFT-synthetic-3k";
const PATH: &str = "/v1/admin/approvals/ap-dlv/decide";

fn draft(text: &str) -> DeliveryDraft {
    DeliveryDraft {
        activation: Revision("set-a".into()),
        target: DeliveryTarget {
            binding: BindingId("team".into()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: "channel-a".into(),
                thread: None,
            },
        },
        answers: vec![
            JsonPayload(format!(r#"{{"text":"{text}"}}"#)),
            JsonPayload(
                r#"{"text":"다음","attachments":[{"type":"text/plain","data":"b25lCnR3bw==","name":"note.txt"}]}"#
                    .into(),
            ),
        ],
    }
}

/// 카드의 «전송 초안» 행 값(승인 문서 원문)과 그 지문.
fn document() -> String {
    draft(SECRET).approval_json().expect("승인 문서")
}

fn seen() -> String {
    hex(&draft(SECRET).sha256().expect("지문"))
}

fn row(card: &Value) -> Option<&str> {
    card["render"]
        .as_array()?
        .iter()
        .find(|r| r["label"] == DELIVERY_CARD_LABEL)
        .and_then(|r| r["value"].as_str())
}

fn card<'a>(list: &'a Value, nonce: &str) -> &'a Value {
    list["items"]
        .as_array()
        .and_then(|v| v.iter().find(|a| a["nonce"] == nonce))
        .unwrap_or_else(|| panic!("카드 없음: {nonce}: {list}"))
}

/// 예약을 만든 사람이 요청자이자 승인자인 전송 승인 카드. `edit`로 카드를 고쳐 어긋난 경우를 만든다.
fn with_card(requester: &str, approver: &str, edit: impl FnOnce(&mut ApprovalView)) -> Fake {
    let fake = Fake::demo();
    fake.edit(|d| {
        let run = d
            .runs
            .iter()
            .find(|r| r.run.0 == "r-1")
            .cloned()
            .expect("run");
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-1")
            .cloned()
            .expect("호출");
        let mut a = approval_view(&call, &run, "ap-dlv", approver);
        a.tool = ToolId(DELIVER_TOOL.into());
        a.tool_revision = Revision(DELIVER_REVISION.into());
        a.call = CallId(DELIVER_CALL.into());
        a.risk = Risk::Irreversible;
        a.requester = pid(requester);
        a.args_sha256 = draft(SECRET).sha256().expect("지문");
        a.render = vec![
            ("제목".into(), "예약 전송 승인".into()),
            ("수신처".into(), "team · channel-a".into()),
            (DELIVERY_CARD_LABEL.into(), document()),
        ];
        edit(&mut a);
        d.approvals.push(a);
    });
    fake
}

fn owner_card() -> Fake {
    with_card("local:guest", "local:guest", |_| {})
}

fn allow(seen: &str) -> String {
    format!(r#"{{"allow":true,"seen":"{seen}"}}"#)
}

fn deny(seen: &str) -> String {
    format!(r#"{{"allow":false,"seen":"{seen}"}}"#)
}

#[test]
fn the_cards_fixture_is_the_real_contract_document() {
    let text = document();
    assert!(
        text.contains(SECRET) && text.contains("one\\ntwo"),
        "{text}"
    );
    let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
    assert_eq!(
        hash,
        draft(SECRET).sha256().unwrap(),
        "행의 SHA-256이 곧 승인 지문이다"
    );
}

#[tokio::test]
async fn the_operator_sees_the_cards_metadata_but_never_the_draft() {
    let s = start(owner_card()).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains(SECRET), "운영자 응답에 초안이 있다");
    assert!(!r.body.contains("channel-a"), "수신처 대화도 초안의 일부다");
    let a = card(&r.json(), "ap-dlv").clone();
    assert_eq!(
        (
            a["tool"].as_str(),
            a["risk"].as_str(),
            a["requester"].as_str(),
            a["approver"].as_str(),
            a["state"].as_str()
        ),
        (
            Some("schedule.deliver"),
            Some("irreversible"),
            Some("local:guest"),
            Some("local:guest"),
            Some("pending")
        )
    );
    assert_eq!(
        a["args_sha256"].as_str(),
        Some(seen().as_str()),
        "지문은 그대로 본다"
    );
    assert_eq!(
        (
            a["render"].as_array().map(Vec::len),
            a["render_hidden"].as_bool(),
            a["approve_blocked"].as_str()
        ),
        (Some(0), Some(true), Some("no_body"))
    );
    // 운영자는 대신 누를 수도 없다(승인자가 아니다).
    let denied = post(&s, OPERATOR, PATH, &allow(&seen())).await;
    assert_eq!(denied.status, 403);
    assert!(!denied.body.contains(SECRET));
}

#[tokio::test]
async fn the_owner_sees_the_exact_draft_approves_it_and_gets_the_same_text_back() {
    let s = start(owner_card()).await;
    let list = get(&s, GUEST, "/v1/admin/approvals").await;
    let a = card(&list.json(), "ap-dlv").clone();
    assert_eq!(
        (a["render_hidden"].as_bool(), a["approve_blocked"].is_null()),
        (Some(false), true)
    );
    let shown = row(&a).expect("전송 초안 행");
    assert_eq!(shown, document(), "승인자는 초안 원문을 그대로 본다");
    let hash: [u8; 32] = Sha256::digest(shown.as_bytes()).into();
    assert_eq!(
        hex(&hash),
        a["args_sha256"].as_str().expect("지문"),
        "보이는 글의 SHA-256이 지문이다"
    );
    let r = post(&s, GUEST, PATH, &allow(&seen())).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["approval"]["state"], "approved");
    assert_eq!(row(&r.json()["approval"]), Some(document().as_str()));
    // 같은 소유자의 재전송은 기존 결과이고, 운영자가 처리된 목록을 봐도 초안은 없다.
    let again = post(&s, GUEST, PATH, &allow(&seen())).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let op = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert!(!op.body.contains(SECRET));
    assert_eq!(
        card(&op.json(), "ap-dlv")["state"],
        "approved",
        "처리 결과(메타데이터)는 보인다"
    );
}

#[tokio::test]
async fn the_draft_never_shows_up_in_runs_audit_status_holds_or_the_map() {
    let s = start(owner_card()).await;
    assert_eq!(post(&s, GUEST, PATH, &allow(&seen())).await.status, 200);
    for path in [
        "/v1/admin/runs?limit=200",
        "/v1/admin/runs/personal/r-1",
        "/v1/admin/holds",
        "/v1/admin/audit?limit=200",
        "/v1/admin/status",
        "/v1/admin/map",
    ] {
        let r = get(&s, OPERATOR, path).await;
        assert_eq!(r.status, 200, "{path}");
        assert!(!r.body.contains(SECRET), "{path}에 초안이 있다");
    }
}

/// 저장소가 초안을 거르지 못해도(버그) 관리 API가 스스로 막는다.
#[tokio::test]
async fn the_api_hides_the_draft_even_when_the_store_leaks_it() {
    let fake = owner_card();
    fake.leak_bodies(true);
    let s = start(fake).await;
    for path in ["/v1/admin/approvals?state=all", "/v1/admin/approvals"] {
        let op = get(&s, OPERATOR, path).await;
        assert!(
            !op.body.contains(SECRET),
            "{path}: 새는 저장소 뒤에서도 운영자에게는 초안이 없어야 한다"
        );
        assert_eq!(card(&op.json(), "ap-dlv")["render_hidden"], true);
    }
    let own = get(&s, GUEST, "/v1/admin/approvals").await;
    assert_eq!(row(card(&own.json(), "ap-dlv")), Some(document().as_str()));
}

#[tokio::test]
async fn a_card_whose_requester_and_approver_differ_hides_the_draft_from_both() {
    let s = start(with_card("local:guest", "local:owner", |_| {})).await;
    for token in [OPERATOR, GUEST] {
        let r = get(&s, token, "/v1/admin/approvals").await;
        assert!(
            !r.body.contains(SECRET),
            "요청자와 승인자가 다르면 아무도 못 본다"
        );
        assert_eq!(card(&r.json(), "ap-dlv")["render_hidden"], true);
    }
}

#[tokio::test]
async fn the_operator_who_created_the_schedule_sees_their_own_card_and_others_do_not() {
    let s = start(with_card("local:owner", "local:owner", |_| {})).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals").await;
    assert_eq!(row(card(&r.json(), "ap-dlv")), Some(document().as_str()));
    let guest = get(&s, GUEST, "/v1/admin/approvals").await;
    assert!(!guest.body.contains(SECRET) && !guest.body.contains("ap-dlv"));
}

/// 승인은 카드의 «전송 초안»을 실제로 볼 수 있을 때만 받는다. 볼 수 없는 내용을 승인하게 되면 안 된다. 거절은 늘 허용한다.
#[tokio::test]
async fn an_approver_who_cannot_see_the_draft_cannot_approve_but_can_deny() {
    let s = start(with_card("local:guest", "local:owner", |_| {})).await;
    let list = get(&s, OPERATOR, "/v1/admin/approvals").await.json();
    assert_eq!(
        (
            card(&list, "ap-dlv")["render_hidden"].as_bool(),
            card(&list, "ap-dlv")["approve_blocked"].as_str()
        ),
        (Some(true), Some("no_body"))
    );
    let blocked = post(&s, OPERATOR, PATH, &allow(&seen())).await;
    assert_eq!(
        (blocked.status, blocked.json()["error"].as_str()),
        (403, Some("no_body"))
    );
    assert!(s.fake.decisions().is_empty(), "브로커까지 가지 않는다");
    assert!(!blocked.body.contains(SECRET));
    let audits = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json()["items"]
        .as_array()
        .map(Vec::len);
    assert_eq!(audits, Some(2), "막힌 승인은 감사 기록을 만들지 않는다");
    let denied = post(&s, OPERATOR, PATH, &deny(&seen())).await;
    assert_eq!(denied.status, 200, "{}", denied.body);
    assert_eq!(denied.json()["approval"]["state"], "denied");
    assert!(!denied.body.contains(SECRET));
}

/// 소유자라도 카드가 승인 지문과 일치하는 초안 원문 전체를 보이지 않으면 눈 감고 승인할 수 없다.
#[tokio::test]
async fn an_owner_cannot_approve_a_card_that_does_not_show_the_exact_draft() {
    let text = document();
    let tampered = text.replacen("channel-a", "channel-b", 1);
    let truncated = text[..text.len() / 2].to_owned();
    let quoted = serde_json::to_string(&text).expect("JSON 문자열");
    let other_draft = draft("another draft").approval_json().expect("다른 초안");
    let cases: Vec<(&str, Edit)> = vec![
        ("행이 없다", Box::new(|a| a.render.clear())),
        (
            "제목만 남았다",
            Box::new(|a| a.render.retain(|(k, _)| k != DELIVERY_CARD_LABEL)),
        ),
        (
            "수신처가 바뀐 글",
            Box::new(move |a| a.render[2].1 = tampered),
        ),
        (
            "앞부분만 보인다",
            Box::new(move |a| a.render[2].1 = truncated),
        ),
        (
            "JSON 문자열로 감쌌다(원문이 아니다)",
            Box::new(move |a| a.render[2].1 = quoted),
        ),
        (
            "다른 초안의 원문",
            Box::new(move |a| a.render[2].1 = other_draft),
        ),
        (
            "초안 행이 둘이다",
            Box::new(|a| {
                a.render
                    .push((DELIVERY_CARD_LABEL.into(), "또 다른 글".into()))
            }),
        ),
        (
            "지문이 다른 초안의 것이다",
            Box::new(|a| a.args_sha256[0] ^= 0xff),
        ),
        (
            "지문의 마지막 바이트만 다르다",
            Box::new(|a| a.args_sha256[31] ^= 1),
        ),
    ];
    for (why, edit) in cases {
        let s = start(with_card("local:guest", "local:guest", edit)).await;
        let a = card(
            &get(&s, GUEST, "/v1/admin/approvals").await.json(),
            "ap-dlv",
        )
        .clone();
        assert_eq!(
            (a["render_hidden"].as_bool(), a["approve_blocked"].as_str()),
            (Some(false), Some("no_body")),
            "{why}: 소유자라도 승인할 수 없다"
        );
        let shown = a["args_sha256"].as_str().expect("지문").to_owned();
        let r = post(&s, GUEST, PATH, &allow(&shown)).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (403, Some("no_body")),
            "{why}: {}",
            r.body
        );
        assert!(
            s.fake.decisions().is_empty(),
            "{why}: 브로커까지 가지 않는다"
        );
        // 거절은 늘 허용한다.
        assert_eq!(
            post(&s, GUEST, PATH, &deny(&shown)).await.status,
            200,
            "{why}"
        );
    }
}

#[tokio::test]
async fn a_card_with_the_exact_draft_is_the_only_one_a_blind_edit_cannot_reach() {
    // 대조: 고치지 않은 카드는 승인된다(위 시험의 어긋남이 카드 구성 탓이 아니라 규칙 탓임을 보인다).
    let s = start(owner_card()).await;
    let a = card(
        &get(&s, GUEST, "/v1/admin/approvals").await.json(),
        "ap-dlv",
    )
    .clone();
    assert!(a["approve_blocked"].is_null());
    assert_eq!(post(&s, GUEST, PATH, &allow(&seen())).await.status, 200);
    assert_eq!(s.fake.decisions().len(), 1);
}
