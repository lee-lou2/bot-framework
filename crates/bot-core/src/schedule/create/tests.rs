use super::*;
use crate::{
    cron::Cron,
    id::BindingId,
    schedule::DeliveryTarget,
    state::{Conversation, ConversationKind},
};
use std::time::{Duration, UNIX_EPOCH};

fn spec(timing: Timing) -> ScheduleSpec {
    ScheduleSpec {
        timing,
        instruction: "확인\n해 줘".into(),
        quiet: false,
        destination: None,
    }
}

fn at(text: &str) -> ScheduleSpec {
    spec(Timing::At(utc::parse_utc(text).unwrap()))
}

fn cron(expression: &str, zone: &str) -> ScheduleSpec {
    spec(Timing::Cron {
        expression: Cron::parse(expression).unwrap(),
        timezone: zone.into(),
    })
}

#[test]
fn the_card_hashes_the_rfc3339_instant_at_the_stored_precision() {
    let first = at("2030-01-01T12:00:00.25Z").seal_create().unwrap();
    assert_eq!(
        first.canonical,
        r#"{"at":"2030-01-01T12:00:00.250Z","instruction":"확인\n해 줘","quiet":false}"#
    );
    assert_eq!(
        format!(
            "{:x}",
            sha2::digest::Output::<sha2::Sha256>::from(first.sha256)
        ),
        "7e0d4934ac11c63b79764f61108dee971c8d0425b1e061a001a4d1854a983b31"
    );
    for equivalent in [
        "2030-01-01T12:00:00.250000000Z",
        "2030-01-01T12:00:00.250999999Z",
    ] {
        assert_eq!(at(equivalent).seal_create().unwrap().sha256, first.sha256);
    }
    assert_ne!(
        at("2030-01-01T12:00:00.251Z").seal_create().unwrap().sha256,
        first.sha256
    );
    assert_eq!(
        at("1970-01-01T00:00:00.000000001Z")
            .seal_create()
            .unwrap()
            .canonical,
        r#"{"at":"1970-01-01T00:00:00Z","instruction":"확인\n해 줘","quiet":false}"#
    );
}

#[test]
fn cron_binds_the_resolved_zone_and_uses_the_same_expression_as_storage() {
    let s = cron("@daily", "Asia/Seoul").seal_create().unwrap();
    assert_eq!(
        s.canonical,
        r#"{"cron":"0 0 * * *","instruction":"확인\n해 줘","quiet":false,"timezone":"Asia/Seoul"}"#
    );
    assert_eq!(
        s.sha256,
        cron("0  0 ? * ?", "Asia/Seoul")
            .seal_create()
            .unwrap()
            .sha256
    );
    assert_ne!(
        s.sha256,
        cron("@daily", "UTC").seal_create().unwrap().sha256
    );
    assert_ne!(
        s.sha256,
        cron("@hourly", "Asia/Seoul").seal_create().unwrap().sha256
    );
    for zone in ["", "asia/seoul", "Not/AZone"] {
        assert!(matches!(
            cron("@daily", zone).seal_create(),
            Err(Error::Invalid)
        ));
    }
}

#[test]
fn the_instruction_quiet_flag_and_exact_recipient_affect_the_hash() {
    let mut s = at("2030-01-01T12:00:00Z");
    let initial = s.seal_create().unwrap().sha256;
    s.quiet = true;
    let quiet = s.seal_create().unwrap().sha256;
    assert_ne!(quiet, initial);
    s.instruction.push('!');
    let changed = s.seal_create().unwrap().sha256;
    assert_ne!(changed, quiet);
    s.destination = Some(DeliveryTarget {
        binding: BindingId("team".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: "channel-a".into(),
            thread: None,
        },
    });
    let directed = s.seal_create().unwrap();
    let value = canon::parse(&directed.canonical).unwrap();
    assert_eq!(
        value["destination"],
        json!({"binding":"team","conversation":{"kind":"channel","id":"channel-a","thread":null}})
    );
    assert_ne!(directed.sha256, changed);
    s.destination.as_mut().unwrap().conversation.thread = Some("thread-a".into());
    assert_ne!(s.seal_create().unwrap().sha256, directed.sha256);
}

#[test]
fn changing_only_the_slack_team_invalidates_both_approvals() {
    use crate::{command::JsonPayload, id::Revision, schedule::DeliveryDraft};

    let hashes = |team: &str| {
        let target = DeliveryTarget {
            binding: BindingId("planning".into()),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: format!("{team}/C1"),
                thread: None,
            },
        };
        let mut schedule = at("2030-01-01T12:00:00Z");
        schedule.destination = Some(target.clone());
        let draft = DeliveryDraft {
            activation: Revision("set-a".into()),
            target,
            answers: vec![JsonPayload(r#"{"text":"준비됐어요"}"#.into())],
        };
        (
            schedule.seal_create().unwrap().sha256,
            draft.sha256().unwrap(),
        )
    };

    let (create, deliver) = hashes("T1");
    let (other_create, other_deliver) = hashes("T2");
    assert_ne!(create, other_create);
    assert_ne!(deliver, other_deliver);
}

#[test]
fn unrepresentable_instants_cannot_alias_a_valid_approval() {
    let last = utc::parse_utc("9999-12-31T23:59:59Z").unwrap();
    assert!(spec(Timing::At(last)).seal_create().is_ok());
    for invalid in [
        UNIX_EPOCH - Duration::from_millis(1),
        last + Duration::from_secs(1),
    ] {
        assert!(matches!(
            spec(Timing::At(invalid)).seal_create(),
            Err(Error::Invalid)
        ));
    }
}
