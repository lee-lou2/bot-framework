use super::*;
use crate::{
    command::JsonPayload,
    id::{BindingId, Revision},
    schedule::DeliveryTarget,
    state::Conversation,
};

fn draft() -> DeliveryDraft {
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
            JsonPayload(r#"{"text":"first reply"}"#.into()),
            JsonPayload(r#"{"text":"next","attachments":[{"type":"text/plain","data":"b25lCnR3bw==","name":"note.txt"}]}"#.into()),
        ],
    }
}

#[test]
fn the_receipt_counts_outbox_rows_even_when_a_reply_contains_an_attachment() {
    let mut d = draft();
    assert_eq!(d.queued_receipt().0.as_bytes(), br#"{"queued":2}"#);
    d.answers.remove(0);
    assert_eq!(d.queued_receipt().0.as_bytes(), br#"{"queued":1}"#);
}

#[test]
fn the_card_and_ledger_use_the_same_readable_document() {
    let document = r#"{"activation":"set-a","answers":[{"text":"first reply"},{"attachments":[{"name":"note.txt","text":"one\ntwo","type":"text/plain"}],"text":"next"}],"target":{"binding":"team","conversation":{"id":"channel-a","kind":"channel","thread":null}}}"#;
    let mut d = draft();
    assert_eq!(d.approval_json().unwrap(), document);
    let hash = d.sha256().unwrap();
    assert_eq!(
        format!("{:x}", sha2::digest::Output::<sha2::Sha256>::from(hash)),
        "d73cd7d58786c688f9ec703ec968732c99e42fce4b98abee6c2e8cbd47f4aa45"
    );
    d.answers[1].0 = r#"{ "attachments": [ { "name":"note.txt", "data":"b25lCnR3bw==","type":"text/plain" } ], "text":"next" }"#.into();
    assert_eq!(d.sha256().unwrap(), hash);
}

#[test]
fn recipient_mapping_thread_reply_order_and_attachment_bytes_each_bind_the_approval() {
    let original = draft().sha256().unwrap();
    for change in [
        |d: &mut DeliveryDraft| d.activation.0.push('b'),
        |d: &mut DeliveryDraft| d.target.binding.0.push('b'),
        |d: &mut DeliveryDraft| d.target.conversation.id.push('b'),
        |d: &mut DeliveryDraft| d.target.conversation.kind = ConversationKind::Direct,
        |d: &mut DeliveryDraft| d.target.conversation.thread = Some("thread-a".into()),
        |d: &mut DeliveryDraft| d.answers.swap(0, 1),
        |d: &mut DeliveryDraft| d.answers[0].0 = r#"{"text":"other reply"}"#.into(),
        |d: &mut DeliveryDraft| d.answers[1].0 = d.answers[1].0.replace("note.txt", "other.txt"),
        |d: &mut DeliveryDraft| d.answers[1].0 = d.answers[1].0.replace("b25lCnR3bw==", "eA=="),
    ] {
        let mut d = draft();
        change(&mut d);
        assert_ne!(d.sha256().unwrap(), original);
    }
}

#[test]
fn one_unreviewable_answer_refuses_the_whole_batch() {
    for answer in [
        r#"{"approval":{},"text":"ok"}"#,
        r#"{"text":"one","text":"two"}"#,
        r#"{"text":"ok","unknown":true}"#,
        r#"{"text":" "}"#,
        r#"{"attachments":[]}"#,
        r#"{"attachments":[{"name":"x","type":"image/png","data":"aGk="}]}"#,
        r#"{"attachments":[{"name":"x","type":"text/plain","data":"/w=="}]}"#,
        r#"{"attachments":[{"name":"x","type":"text/plain","data":"AA=="}]}"#,
        r#"{"attachments":[{"name":"x","type":"text/plain","data":"Zh=="}]}"#,
        r#"{"attachments":[{"name":"x","type":"text/plain","data":"aGk=","url":"x"}]}"#,
        r#"{"attachments":[{"name":"x","type":"text/plain","text":"fake"}]}"#,
    ] {
        let mut d = draft();
        d.answers.push(JsonPayload(answer.into()));
        assert!(matches!(d.sha256(), Err(Error::Invalid)), "{answer}");
    }
    let mut d = draft();
    d.answers.push(JsonPayload(
        json!({"text":"x".repeat(CARD_CAPACITY)}).to_string(),
    ));
    assert!(matches!(d.approval_json(), Err(Error::Invalid)));
    d.answers.clear();
    assert!(matches!(d.sha256(), Err(Error::Invalid)));
}

#[test]
fn a_pull_or_unaddressable_destination_cannot_be_approved_for_external_delivery() {
    for kind in [
        ConversationKind::Cli,
        ConversationKind::Http,
        ConversationKind::Scheduled,
    ] {
        let mut d = draft();
        d.target.conversation.kind = kind;
        assert!(matches!(d.sha256(), Err(Error::Invalid)));
    }
    let mut d = draft();
    d.target.conversation.thread = Some(String::new());
    assert!(matches!(d.sha256(), Err(Error::Invalid)));
}
