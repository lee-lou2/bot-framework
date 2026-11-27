use super::*;
use crate::service::schedule::args::{Create, Reject, parse_cancel, render_list};
use bot_core::{
    cron::Cron,
    id::BindingId,
    port::Error,
    schedule::{DeliveryTarget, MAX_INSTRUCTION_BYTES, ScheduleSummary, Timing, utc::parse_utc},
    state::{Conversation, ConversationKind},
};

fn args(text: &str) -> JsonPayload {
    JsonPayload(text.into())
}

fn parsed(args: &JsonPayload, zone: &str) -> Result<(Create, crate::ports::Sealed), Reject> {
    let mut create = Create::parse(args, zone)?;
    let sealed = create.seal(None)?;
    Ok((create, sealed))
}

fn at_2030() -> &'static str {
    r#""at":"2030-01-01T12:00:00Z""#
}

#[test]
fn create_arguments_are_read_strictly() {
    let good = [
        format!(r#"{{{},"instruction":"check the mail"}}"#, at_2030()),
        format!(r#"{{{},"instruction":"x","quiet":true}}"#, at_2030()),
        r#"{"cron":"0 9 * * 1-5","instruction":"standup"}"#.to_owned(),
        r#"{"cron":"0 9 * * 1-5","timezone":"Asia/Seoul","instruction":"standup"}"#.to_owned(),
    ];
    for text in good {
        assert!(parsed(&args(&text), "UTC").is_ok(), "{text}");
    }
    let invalid = [
        "[]",
        "{}",
        r#"{"instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","cron":"* * * * *","instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","timezone":"UTC","instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":""}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"   "}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":5}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","quiet":"yes"}"#,
        r#"{"at":5,"instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","extra":1}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","instruction":"y"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"a\u0000b"}"#,
    ];
    for text in invalid {
        assert!(
            matches!(parsed(&args(text), "UTC"), Err(Reject::Invalid)),
            "{text}"
        );
    }
    let big = format!(
        r#"{{{},"instruction":"{}"}}"#,
        at_2030(),
        "a".repeat(MAX_INSTRUCTION_BYTES + 1)
    );
    assert!(matches!(parsed(&args(&big), "UTC"), Err(Reject::Invalid)));
    let edge = format!(
        r#"{{{},"instruction":"{}"}}"#,
        at_2030(),
        "a".repeat(MAX_INSTRUCTION_BYTES)
    );
    assert!(parsed(&args(&edge), "UTC").is_ok());
}

#[test]
fn a_destination_is_read_strictly_and_only_the_registry_can_make_it_real() {
    let with = |destination: &str| {
        Create::parse(
            &args(&format!(
                r#"{{{},"instruction":"x","destination":{destination}}}"#,
                at_2030()
            )),
            "UTC",
        )
    };
    let create = with(
        r##"{"binding":"slack","conversation":{"kind":"channel","id":"#general","thread":"t1"}}"##,
    )
    .unwrap();
    let target = create.requested().expect("requested destination");
    assert_eq!(
        (
            target.binding.0.as_str(),
            target.conversation.id.as_str(),
            target.conversation.thread.as_deref()
        ),
        ("slack", "#general", Some("t1"))
    );
    assert!(with(r#"{"binding":"slack","conversation":{"kind":"direct","id":"U1"}}"#).is_ok());
    assert!(
        Create::parse(
            &args(&format!(r#"{{{},"instruction":"x"}}"#, at_2030())),
            "UTC"
        )
        .unwrap()
        .requested()
        .is_none()
    );
    for bad in [
        r#"{"binding":"slack"}"#,
        r#"{"conversation":{"kind":"direct","id":"U1"}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct"}}"#,
        r#"{"binding":"slack","conversation":{"kind":"cli","id":"U1"}}"#,
        r#"{"binding":"slack","conversation":{"kind":"scheduled","id":"U1"}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct","id":""}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct","id":"U1","extra":1}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct","id":"U1","thread":5}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct","id":"U\n1"}}"#,
        r#"{"binding":"Not A Name","conversation":{"kind":"direct","id":"U1"}}"#,
        r#"{"binding":"slack","conversation":{"kind":"direct","id":"U1"},"extra":1}"#,
        r#"{"binding":"slack","conversation":"U1"}"#,
        r#""U1""#,
        "[]",
        "null",
    ] {
        assert!(matches!(with(bad), Err(Reject::Invalid)), "{bad}");
    }
    // Only the registry-resolved canonical destination is sealed into the card and hash.
    let mut create = with(
        r##"{"binding":"slack","conversation":{"kind":"channel","id":"#general","thread":"t1"}}"##,
    )
    .unwrap();
    let canonical = DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: "T1/C1".into(),
            thread: Some("t1".into()),
        },
    };
    let sealed = create.seal(Some(canonical)).unwrap();
    assert!(
        sealed.canonical.contains(r#""destination":{"binding":"slack","conversation":{"id":"T1/C1","kind":"channel","thread":"t1"}}"#),
        "{}",
        sealed.canonical
    );
    assert!(!sealed.canonical.contains("#general"));
    let plain = parsed(
        &args(&format!(r#"{{{},"instruction":"x"}}"#, at_2030())),
        "UTC",
    )
    .unwrap()
    .1;
    assert_ne!(sealed.sha256, plain.sha256);
}

#[test]
fn timing_checks_wait_until_the_record_is_known_not_to_be_a_replay() {
    let now = parse_utc("2030-01-01T00:00:00Z").unwrap();
    let spec = |text: &str, zone: &str| {
        parsed(&args(text), zone)
            .map_err(|_| Error::Invalid)
            .and_then(|(create, _)| create.spec(now))
    };
    // Accepts timestamps strictly after `now` (at millisecond granularity) up to 1,830 days out.
    assert!(spec(r#"{"at":"2029-12-31T23:59:59Z","instruction":"x"}"#, "UTC").is_err());
    assert!(spec(r#"{"at":"2030-01-01T00:00:00Z","instruction":"x"}"#, "UTC").is_err());
    assert!(
        spec(
            r#"{"at":"2030-01-01T00:00:00.0009Z","instruction":"x"}"#,
            "UTC"
        )
        .is_err()
    );
    assert!(spec(r#"{"at":"2030-01-01T00:00:01Z","instruction":"x"}"#, "UTC").is_ok());
    let limit = now + std::time::Duration::from_secs(1_830 * 24 * 3600);
    let at = |time| {
        format!(
            r#"{{"at":"{}","instruction":"x"}}"#,
            bot_core::schedule::utc::format_utc(time)
        )
    };
    assert!(spec(&at(limit), "UTC").is_ok());
    assert!(spec(&at(limit + std::time::Duration::from_millis(1)), "UTC").is_err());
    assert!(spec(r#"{"at":"tomorrow","instruction":"x"}"#, "UTC").is_err());
    assert!(
        spec(
            r#"{"at":"2030-01-02T00:00:00+09:00","instruction":"x"}"#,
            "UTC"
        )
        .is_err()
    );
    assert!(spec(r#"{"cron":"not a cron","instruction":"x"}"#, "UTC").is_err());
    let cron = spec(r#"{"cron":"0 9 * * *","instruction":"x"}"#, "Asia/Seoul").unwrap();
    assert!(matches!(&cron.timing, Timing::Cron { timezone, .. } if timezone == "Asia/Seoul"));
    let pinned = spec(
        r#"{"cron":"0 9 * * *","timezone":"America/New_York","instruction":"x"}"#,
        "Asia/Seoul",
    )
    .unwrap();
    assert!(
        matches!(&pinned.timing, Timing::Cron { timezone, .. } if timezone == "America/New_York")
    );
    for bad in ["has space", "", "a;b", "../etc"] {
        let text = format!(r#"{{"cron":"0 9 * * *","timezone":"{bad}","instruction":"x"}}"#);
        assert!(spec(&text, "UTC").is_err(), "{bad}");
    }
    assert!(spec(r#"{"cron":"0 9 * * *","instruction":"x"}"#, "bad zone").is_err());
}

#[test]
fn the_omitted_timezone_is_filled_before_sealing_so_the_hash_depends_on_it() {
    let seal = |zone: &str| {
        parsed(&args(r#"{"cron":"0 9 * * *","instruction":"x"}"#), zone)
            .unwrap()
            .1
    };
    let (seoul, utc) = (seal("Asia/Seoul"), seal("UTC"));
    assert!(
        seoul.canonical.contains(r#""timezone":"Asia/Seoul""#),
        "{}",
        seoul.canonical
    );
    assert!(utc.canonical.contains(r#""timezone":"UTC""#));
    assert_ne!(
        seoul.sha256, utc.sha256,
        "resolved timezone must affect the sealed hash"
    );
    let explicit = |zone: &str| {
        parsed(
            &args(r#"{"cron":"0 9 * * *","timezone":"Asia/Seoul","instruction":"x"}"#),
            zone,
        )
        .unwrap()
        .1
    };
    assert_eq!(explicit("UTC").sha256, explicit("America/New_York").sha256);
    assert_eq!(
        explicit("UTC").sha256,
        seoul.sha256,
        "explicit timezone must hash identically to the same defaulted timezone"
    );
    let at = parsed(
        &args(r#"{"at":"2030-01-01T12:00:00Z","instruction":"x"}"#),
        "Asia/Seoul",
    )
    .unwrap()
    .1;
    assert!(!at.canonical.contains("timezone"));
}

#[test]
fn the_card_text_and_hash_are_the_cores_seal_of_the_parsed_spec() {
    let seal = |text: &str| parsed(&args(text), "UTC").unwrap().1;
    let plain = seal(r#"{"at":"2030-01-01T12:00:00Z","instruction":"check the mail"}"#);
    assert_eq!(
        plain.canonical,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"check the mail","quiet":false}"#
    );
    assert!(plain.destination.is_none());
    for same in [
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"check the mail","quiet":false}"#,
        r#"{"at":"2030-01-01T12:00:00.000Z","instruction":"check the mail"}"#,
        r#"{"at":"2030-01-01T12:00:00.000000000Z","quiet":false,"instruction":"check the mail"}"#,
    ] {
        let sealed = seal(same);
        assert_eq!(
            (sealed.canonical.as_str(), sealed.sha256),
            (plain.canonical.as_str(), plain.sha256),
            "{same}"
        );
    }
    for other in [
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"check the mail","quiet":true}"#,
        r#"{"at":"2030-01-01T12:00:00.001Z","instruction":"check the mail"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"check the mail "}"#,
    ] {
        assert_ne!(seal(other).sha256, plain.sha256, "{other}");
    }
    let daily = seal(r#"{"cron":"@daily","timezone":"Asia/Seoul","instruction":"x"}"#);
    assert_eq!(
        daily.canonical,
        r#"{"cron":"0 0 * * *","instruction":"x","quiet":false,"timezone":"Asia/Seoul"}"#
    );
    assert_eq!(
        seal(r#"{"cron":"0  0 * * *","timezone":"Asia/Seoul","instruction":"x"}"#).sha256,
        daily.sha256
    );
}

#[test]
fn sealing_never_reads_the_clock_and_the_future_check_belongs_to_new_calls_only() {
    // Past timestamps still seal identically so replays of completed calls match their receipt hash.
    let text = r#"{"at":"2020-01-01T00:00:00Z","instruction":"x"}"#;
    let (create, sealed) = parsed(&args(text), "UTC").unwrap();
    assert_eq!(
        sealed.canonical,
        r#"{"at":"2020-01-01T00:00:00Z","instruction":"x","quiet":false}"#
    );
    assert!(matches!(
        create.spec(parse_utc("2030-01-01T00:00:00Z").unwrap()),
        Err(Error::Invalid)
    ));
    let (create, again) = parsed(&args(text), "UTC").unwrap();
    assert_eq!(again.sha256, sealed.sha256);
    assert!(
        create
            .spec(parse_utc("2019-01-01T00:00:00Z").unwrap())
            .is_ok()
    );
    for zone in ["asia/seoul", "Mars/Base", "UTC+9", "Asia/Seoul "] {
        let text = format!(r#"{{"cron":"0 9 * * *","timezone":"{zone}","instruction":"x"}}"#);
        assert!(
            matches!(parsed(&args(&text), "UTC"), Err(Reject::Invalid)),
            "{zone}"
        );
    }
    assert!(matches!(
        parsed(
            &args(r#"{"cron":"0 9 * * *","instruction":"x"}"#),
            "Nowhere"
        ),
        Err(Reject::Invalid)
    ));
}

#[test]
fn cancel_arguments_are_one_clean_id_and_the_hash_follows_the_canonical_form() {
    assert!(parse_cancel(&args(r#"{"id":"sched-001"}"#)).is_ok());
    for bad in [
        "{}",
        r#"{"id":""}"#,
        r#"{"id":"a b"}"#,
        r#"{"id":5}"#,
        r#"{"id":"x","extra":1}"#,
        r#"{"id":"x","id":"x"}"#,
    ] {
        assert!(
            matches!(parse_cancel(&args(bad)), Err(Reject::Invalid)),
            "{bad}"
        );
    }
    let a = parsed(
        &args(r#"{"at":"2030-01-01T12:00:00Z","instruction":"x"}"#),
        "UTC",
    )
    .unwrap()
    .1;
    let b = parsed(
        &args("{ \"instruction\": \"x\",\n \"at\": \"2030-01-01T12:00:00Z\" }"),
        "UTC",
    )
    .unwrap()
    .1;
    assert_eq!(
        (a.canonical.as_str(), a.sha256),
        (b.canonical.as_str(), b.sha256)
    );
}

#[test]
fn the_list_never_carries_instructions_or_recipients() {
    let at = parse_utc("2030-01-01T12:00:00Z").unwrap();
    let payload = render_list(&[
        ScheduleSummary {
            id: ScheduleId("s1".into()),
            timing: Timing::At(at),
            quiet: true,
        },
        ScheduleSummary {
            id: ScheduleId("s2".into()),
            timing: Timing::Cron {
                expression: Cron::parse("0 9 * * 1-5").unwrap(),
                timezone: "Asia/Seoul".into(),
            },
            quiet: false,
        },
    ])
    .unwrap();
    assert_eq!(
        payload.0,
        r#"{"schedules":[{"at":"2030-01-01T12:00:00Z","id":"s1","quiet":true},{"cron":"0 9 * * 1-5","id":"s2","quiet":false,"timezone":"Asia/Seoul"}]}"#
    );
    assert_eq!(render_list(&[]).unwrap().0, r#"{"schedules":[]}"#);
}

#[test]
fn the_host_only_id_is_reserved_but_not_a_model_tool() {
    assert!(is_builtin(DELIVER_TOOL) && is_builtin(CREATE_TOOL));
    assert!(!is_builtin("schedule.other") && !is_builtin("memory.share"));
    let scopes = Scopes::from([
        CREATE_TOOL.into(),
        LIST_TOOL.into(),
        CANCEL_TOOL.into(),
        DELIVER_TOOL.into(),
    ]);
    let ids: Vec<_> = bot_core::builtin::model_tools(&scopes)
        .map(|spec| spec.id)
        .collect();
    assert_eq!(ids, [CREATE_TOOL, LIST_TOOL, CANCEL_TOOL]);
    assert!(mutating_tool(LIST_TOOL).is_none() && mutating_tool(DELIVER_TOOL).is_none());
}

#[test]
fn issued_builtin_ids_and_revisions_are_stable() {
    use crate::service::{
        deliver::deliver_tool,
        preflight::DELEGATE_TOOL,
        share::{SHARE_TOOL, share_tool},
    };
    for (id, declaration, issued) in [
        (SHARE_TOOL, share_tool(), "memory.share"),
        (CREATE_TOOL, create_tool(), "schedule.create"),
        (LIST_TOOL, list_tool(), "schedule.list"),
        (CANCEL_TOOL, cancel_tool(), "schedule.cancel"),
        (DELIVER_TOOL, deliver_tool(), "schedule.deliver"),
    ] {
        assert_eq!(id.as_bytes(), issued.as_bytes());
        assert_eq!(declaration.revision.0.as_bytes(), b"builtin-1");
    }
    assert_eq!(DELEGATE_TOOL.as_bytes(), b"delegate");
}
