use super::*;
use std::time::Duration;

fn delivery(raw: Raw) -> Delivery {
    match step(raw, |v| receipt_of("C1", v)) {
        Step::Done(delivery) => delivery,
        Step::Format => Delivery::Rejected,
    }
}

#[test]
fn outcomes_follow_the_documented_classification() {
    assert_eq!(
        delivery(Raw::Ok(json!({"channel": "C1", "ts": "1.5"}))),
        Delivery::Sent {
            receipt: "C1:1.5".into()
        }
    );
    assert_eq!(delivery(Raw::Ok(json!({}))), Delivery::Held);
    for reply in [
        json!({"channel": "C1", "ts": ""}),
        json!({"channel": "C1", "ts": "later"}),
        json!({"channel": "C1"}),
        json!({"ts": "1.5"}),
        json!({"channel": "C2", "ts": "1.5"}),
        json!({"channel": "c1", "ts": "1.5"}),
        json!({"channel": 7, "ts": "1.5"}),
    ] {
        assert_eq!(delivery(Raw::Ok(reply.clone())), Delivery::Held, "{reply}");
    }
    assert_eq!(
        delivery(Raw::Error("channel_not_found".into())),
        Delivery::Rejected
    );
    assert_eq!(
        delivery(Raw::Error("not_in_channel".into())),
        Delivery::Rejected
    );
    assert_eq!(
        delivery(Raw::Error("internal_error".into())),
        Delivery::Held
    );
    assert_eq!(delivery(Raw::Unknown), Delivery::Held);
    // Token rejection happens pre-execution and is retryable after credential refresh.
    assert_eq!(
        delivery(Raw::TokenRejected),
        Delivery::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None
        }
    );
    assert_eq!(
        delivery(Raw::BeforeWrite),
        Delivery::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None
        }
    );
    assert_eq!(
        delivery(Raw::RateLimited(Duration::from_secs(9))),
        Delivery::NotSent {
            reason: NotSentReason::RateLimited,
            retry_after: Some(Duration::from_secs(9))
        }
    );
    for code in FORMAT_ERRORS {
        assert!(
            matches!(step(Raw::Error(code.into()), |_| None), Step::Format),
            "{code}"
        );
    }
}

#[test]
fn thread_timestamps_must_look_like_slack_ts() {
    for ok in ["1700000000.000100", "1.1", "123456789012.123456789"] {
        assert!(is_ts(ok), "{ok}");
    }
    for bad in [
        "",
        "1",
        ".1",
        "1.",
        "a.1",
        "1.b",
        "1.2.3",
        "1234567890123.1",
        "1.1234567890",
        "1 .1",
    ] {
        assert!(!is_ts(bad), "{bad}");
    }
}

#[test]
fn an_upload_receipt_needs_exactly_the_files_we_sent_with_nonempty_unique_ids() {
    let receipt = |reply: Value, asked: &[&str]| files_receipt("C1", &reply, asked);
    assert_eq!(
        receipt(
            json!({"files": [{"id": "F2"}, {"id": "F1", "title": "x"}]}),
            &["F1", "F2"]
        )
        .as_deref(),
        Some("C1:F1,F2")
    );
    for (reply, asked) in [
        (json!({}), vec!["F1"]),
        (json!({"files": []}), vec!["F1"]),
        (json!({"files": [{"id": "F1"}]}), vec!["F1", "F2"]),
        (
            json!({"files": [{"id": "F1"}, {"id": "F9"}]}),
            vec!["F1", "F2"],
        ),
        (json!({"files": [{"title": "no id"}]}), vec!["F1"]),
        (
            json!({"files": [{"id": "F1"}, {"id": "F2"}, {"id": "F3"}]}),
            vec!["F1", "F2"],
        ),
        (
            json!({"files": [{"id": "F1"}, {"title": "extra"}]}),
            vec!["F1", "F2"],
        ),
        (json!({"files": [{"id": "F1"}, {}]}), vec!["F1"]),
        (json!({"files": [{"id": ""}]}), vec![""]),
        (
            json!({"files": [{"id": "F1"}, {"id": "F1"}]}),
            vec!["F1", "F1"],
        ),
        (
            json!({"files": [{"id": "F1"}, {"id": "F1"}]}),
            vec!["F1", "F2"],
        ),
        (json!({"files": [{"id": 1}]}), vec!["F1"]),
    ] {
        assert_eq!(receipt(reply.clone(), &asked), None, "{reply} {asked:?}");
    }
}

#[test]
fn only_the_truncation_warning_holds_a_successful_post() {
    let reply = |warnings: Value| json!({"ok": true, "channel": "C1", "ts": "1.5", "response_metadata": {"warnings": warnings}});
    assert!(truncated(&reply(json!(["message_truncated"]))));
    assert!(truncated(&reply(json!([
        "missing_charset",
        "message_truncated"
    ]))));
    assert!(!truncated(&reply(json!(["missing_charset"]))));
    assert!(!truncated(&reply(json!([]))));
    assert!(!truncated(
        &json!({"ok": true, "channel": "C1", "ts": "1.5"})
    ));
    assert!(!truncated(&reply(json!("message_truncated"))));
}

#[test]
fn dm_approval_notices_and_cards_are_not_final_replies() {
    let text = |s: &str| ReplyBody {
        text: Some(s.into()),
        approval: None,
        attachments: vec![],
    };
    assert!(is_final_reply(&text("Done.")));
    assert!(!is_final_reply(&text(DM_SENT_NOTICE)));
    assert!(!is_final_reply(&text(DM_UNAVAILABLE_NOTICE)));
    assert!(!is_final_reply(&text(DM_UNCONFIRMED_NOTICE)));
    assert!(!is_final_reply(&ReplyBody {
        text: None,
        approval: Some(crate::reply::Card {
            nonce: "n1".into(),
            title: "Approval".into(),
            lines: vec![],
        }),
        attachments: vec![],
    }));
}
