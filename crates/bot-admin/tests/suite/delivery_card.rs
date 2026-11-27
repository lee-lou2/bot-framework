//! Scheduled delivery card (`schedule.deliver`): owner-only draft visibility and exact SHA-256 approval binding.

use crate::support::*;
use bot_admin::{
    fake::{Fake, approval_view, pid},
    json::hex,
};
use bot_core::{
    admin::ApprovalView,
    cards::{LABEL_RECIPIENT, LABEL_TITLE},
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
                r#"{"text":"next","attachments":[{"type":"text/plain","data":"b25lCnR3bw==","name":"note.txt"}]}"#
                    .into(),
            ),
        ],
    }
}

fn document() -> String {
    draft(SECRET).approval_json().expect("approval document")
}

fn seen() -> String {
    hex(&draft(SECRET).sha256().expect("digest"))
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
        .unwrap_or_else(|| panic!("missing card {nonce}: {list}"))
}

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
            .expect("call");
        let mut a = approval_view(&call, &run, "ap-dlv", approver);
        a.tool = ToolId(DELIVER_TOOL.into());
        a.tool_revision = Revision(DELIVER_REVISION.into());
        a.call = CallId(DELIVER_CALL.into());
        a.risk = Risk::Irreversible;
        a.requester = pid(requester);
        a.args_sha256 = draft(SECRET).sha256().expect("digest");
        a.render = vec![
            (LABEL_TITLE.into(), "Scheduled delivery approval".into()),
            (LABEL_RECIPIENT.into(), "team · channel-a".into()),
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
        "SHA-256 of rendered draft row must equal args_sha256"
    );
}

#[tokio::test]
async fn the_operator_sees_the_cards_metadata_but_never_the_draft() {
    let s = start(owner_card()).await;
    let r = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert_eq!(r.status, 200);
    assert!(!r.body.contains(SECRET), "operator response leaked draft");
    assert!(
        !r.body.contains("channel-a"),
        "destination conversation is part of the private draft"
    );
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
    assert_eq!(a["args_sha256"].as_str(), Some(seen().as_str()));
    assert_eq!(
        (
            a["render"].as_array().map(Vec::len),
            a["render_hidden"].as_bool(),
            a["approve_blocked"].as_str()
        ),
        (Some(0), Some(true), Some("no_body"))
    );
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
    let shown = row(&a).expect("delivery draft row");
    assert_eq!(shown, document());
    let hash: [u8; 32] = Sha256::digest(shown.as_bytes()).into();
    assert_eq!(hex(&hash), a["args_sha256"].as_str().expect("digest"));
    let r = post(&s, GUEST, PATH, &allow(&seen())).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["approval"]["state"], "approved");
    assert_eq!(row(&r.json()["approval"]), Some(document().as_str()));
    let again = post(&s, GUEST, PATH, &allow(&seen())).await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true))
    );
    let op = get(&s, OPERATOR, "/v1/admin/approvals?state=all").await;
    assert!(!op.body.contains(SECRET));
    assert_eq!(card(&op.json(), "ap-dlv")["state"], "approved");
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
        assert!(!r.body.contains(SECRET), "draft leaked on {path}");
    }
}

/// Defense-in-depth: the admin API strips the draft even if the store fails to filter it.
#[tokio::test]
async fn the_api_hides_the_draft_even_when_the_store_leaks_it() {
    let fake = owner_card();
    fake.leak_bodies(true);
    let s = start(fake).await;
    for path in ["/v1/admin/approvals?state=all", "/v1/admin/approvals"] {
        let op = get(&s, OPERATOR, path).await;
        assert!(
            !op.body.contains(SECRET),
            "{path}: operator must not see draft behind a leaky store"
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
            "neither principal may view draft when requester != approver"
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
    assert!(s.fake.decisions().is_empty(), "must not reach broker");
    assert!(!blocked.body.contains(SECRET));
    let audits = get(&s, OPERATOR, "/v1/admin/audit?limit=200").await.json()["items"]
        .as_array()
        .map(Vec::len);
    assert_eq!(audits, Some(2), "blocked approval must not append audit");
    let denied = post(&s, OPERATOR, PATH, &deny(&seen())).await;
    assert_eq!(denied.status, 200, "{}", denied.body);
    assert_eq!(denied.json()["approval"]["state"], "denied");
    assert!(!denied.body.contains(SECRET));
}

#[tokio::test]
async fn an_owner_cannot_approve_a_card_that_does_not_show_the_exact_draft() {
    let text = document();
    let tampered = text.replacen("channel-a", "channel-b", 1);
    let truncated = text[..text.len() / 2].to_owned();
    let quoted = serde_json::to_string(&text).expect("JSON string");
    let other_draft = draft("another draft").approval_json().expect("other draft");
    let cases: Vec<(&str, Edit)> = vec![
        ("no rows", Box::new(|a| a.render.clear())),
        (
            "title only",
            Box::new(|a| a.render.retain(|(k, _)| k != DELIVERY_CARD_LABEL)),
        ),
        (
            "modified recipient",
            Box::new(move |a| a.render[2].1 = tampered),
        ),
        (
            "truncated draft",
            Box::new(move |a| a.render[2].1 = truncated),
        ),
        (
            "JSON-quoted instead of raw",
            Box::new(move |a| a.render[2].1 = quoted),
        ),
        (
            "different draft text",
            Box::new(move |a| a.render[2].1 = other_draft),
        ),
        (
            "duplicate draft row",
            Box::new(|a| {
                a.render
                    .push((DELIVERY_CARD_LABEL.into(), "another text".into()))
            }),
        ),
        (
            "mismatched first digest byte",
            Box::new(|a| a.args_sha256[0] ^= 0xff),
        ),
        (
            "mismatched last digest byte",
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
            "{why}: owner cannot approve incomplete or mismatched draft"
        );
        let shown = a["args_sha256"].as_str().expect("digest").to_owned();
        let r = post(&s, GUEST, PATH, &allow(&shown)).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str()),
            (403, Some("no_body")),
            "{why}: {}",
            r.body
        );
        assert!(
            s.fake.decisions().is_empty(),
            "{why}: must not reach broker"
        );
        assert_eq!(
            post(&s, GUEST, PATH, &deny(&shown)).await.status,
            200,
            "{why}"
        );
    }
}

#[tokio::test]
async fn a_card_with_the_exact_draft_is_the_only_one_a_blind_edit_cannot_reach() {
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
