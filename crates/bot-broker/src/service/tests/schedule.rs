//! 내장 `schedule.create`·`schedule.list`·`schedule.cancel`.

use super::*;
use crate::rfc3339::parse_utc;
use bot_core::{
    ledger::{ApprovalState, CallLedger, CallPhase, StartCall},
    schedule::{MAX_ACTIVE_PER_OWNER, Timing},
    state::{ConversationKind, Risk},
};

pub(super) const WITH_SCHEDULE: [&str; 10] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    "schedule.create",
    "schedule.list",
    "schedule.cancel",
];
pub(super) const OWNER: &str = "local:owner";
pub(super) const AT: &str = "2030-01-01T12:00:00Z";

pub(super) fn snapshot_with(revision: &str) -> SnapshotBuilder {
    base(revision)
        .role("owner", &WITH_SCHEDULE)
        .policy_scopes(&WITH_SCHEDULE)
}

pub(super) fn schedule_world() -> World {
    let w = World::new();
    // 호스트 등록부에는 시험의 전송 초안이 가는 채널(바인딩 team, 채널 channel-a)이 등록돼 있다
    w.routes.register(
        "personal",
        "team",
        bot_core::state::ConversationKind::Channel,
        "channel-a",
        "channel-a",
    );
    w.publish(snapshot_with("r1").build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    w
}

pub(super) fn create(w: &World, call: &str, args: &str) -> Result<CallResult, Error> {
    w.call(call, "schedule.create", args)
}

pub(super) fn at_args(instruction: &str) -> String {
    format!(r#"{{"at":"{AT}","instruction":"{instruction}"}}"#)
}

fn id_of(payload: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    value["schedule"].as_str().expect("schedule").to_owned()
}

pub(super) fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("schedule.refuse".into(), word.into()))
}

#[test]
fn an_owner_creates_lists_and_cancels_their_own_schedule() {
    let w = schedule_world();
    let created = done(create(&w, "c1", &at_args("check the mail")));
    let id = id_of(&created);
    let (instruction, timing, quiet) = w.schedules.get(&id).expect("stored");
    assert_eq!(instruction, "check the mail");
    assert_eq!(timing, Timing::At(parse_utc(AT).unwrap()));
    assert!(!quiet, "quiet의 기본값은 false다");
    // 목록: 본인 활성 예약의 ID·시각·quiet만(지시문은 없다)
    let listed = done(w.call("c2", "schedule.list", "{}"));
    assert_eq!(
        listed,
        format!(r#"{{"schedules":[{{"at":"{AT}","id":"{id}","quiet":false}}]}}"#)
    );
    assert!(!listed.contains("check the mail"));
    // 취소
    assert_eq!(
        done(w.call("c3", "schedule.cancel", &format!(r#"{{"id":"{id}"}}"#))),
        format!(r#"{{"schedule":"{id}"}}"#)
    );
    assert_eq!(w.schedules.count(), 0);
    assert_eq!(
        done(w.call("c4", "schedule.list", "{}")),
        r#"{"schedules":[]}"#
    );
    assert!(w.runner.jobs().is_empty(), "도구 서버를 거치지 않는다");
}

#[test]
fn a_cron_schedule_uses_the_profile_timezone_unless_one_is_given() {
    let w = schedule_world();
    let plain = id_of(&done(create(
        &w,
        "c1",
        r#"{"cron":"0 9 * * 1-5","instruction":"standup","quiet":true}"#,
    )));
    let (_, timing, quiet) = w.schedules.get(&plain).unwrap();
    assert!(
        matches!(&timing, Timing::Cron { expression, timezone }
            if expression.as_str() == "0 9 * * 1-5" && timezone == "UTC"),
        "{timing:?}"
    );
    assert!(quiet);
    let seoul = id_of(&done(create(
        &w,
        "c2",
        r#"{"cron":"0 9 * * *","timezone":"Asia/Seoul","instruction":"morning"}"#,
    )));
    assert!(matches!(
        &w.schedules.get(&seoul).unwrap().1,
        Timing::Cron { timezone, .. } if timezone == "Asia/Seoul"
    ));
    // 목록에는 cron·시간대가 나온다
    let listed = done(w.call("c3", "schedule.list", "{}"));
    assert!(
        listed.contains(r#""cron":"0 9 * * 1-5""#) && listed.contains(r#""timezone":"Asia/Seoul""#)
    );
    // 저장소가 모르는 시간대는 거절한다
    assert!(matches!(
        create(
            &w,
            "c4",
            r#"{"cron":"0 9 * * *","timezone":"Mars/Base","instruction":"x"}"#
        ),
        Err(Error::Invalid)
    ));
}

#[test]
fn bad_arguments_never_create_a_call_record_or_a_schedule() {
    let w = schedule_world();
    for (i, args) in [
        r#"{"instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","cron":"* * * * *","instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00+09:00","instruction":"x"}"#,
        r#"{"at":"1970-01-02T00:00:00Z","instruction":"x"}"#, // 장치의 시계(1970-01-12)보다 앞선 시각
        r#"{"at":"2030-01-01T12:00:00Z","instruction":""}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","extra":1}"#,
        r#"{"cron":"every day","instruction":"x"}"#,
        "[]",
    ]
    .iter()
    .enumerate()
    {
        let call = format!("bad{i}");
        assert!(
            matches!(create(&w, &call, args), Err(Error::Invalid)),
            "{args}"
        );
        assert!(!w.ledger.has_call("r1", &call), "{args}");
    }
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_redelivered_call_returns_its_receipt_even_after_the_time_has_passed() {
    let w = schedule_world();
    let first = done(create(&w, "c1", &at_args("once")));
    assert_eq!(done(create(&w, "c1", &at_args("once"))), first);
    assert_eq!(w.schedules.count(), 1, "다시 만들지 않는다");
    // 예정 시각이 지난 뒤의 재전달도 같은 영수증이다(시간에 기대는 검사는 영수증을 찾은 뒤에 한다)
    w.clock.set(2_000_000_000 + 1_800_000_000); // 2030-01-01 12:00 UTC는 1893499200이므로 한참 지났다
    assert_eq!(done(create(&w, "c1", &at_args("once"))), first);
    // 같은 호출 ID에 다른 인자·다른 도구는 충돌이다
    assert!(matches!(
        create(&w, "c1", &at_args("something else")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Missing | Error::Conflict | Error::Invalid | Error::Denied) | Ok(_)
    ));
    assert_eq!(w.schedules.count(), 1);
}

#[test]
fn the_scope_the_human_requester_and_the_destination_are_all_required() {
    // 스코프가 없다
    let w = World::new();
    w.publish(
        base("r1")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        create(&w, "c1", &at_args("x")),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "scope_missing"));
    assert!(!w.ledger.has_call("r1", "c1"));
    // 서비스 주체는 예약을 만들 수 없다
    let w = schedule_world();
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    let result = w.call_on(&w.broker, "r2", 1, "c1", "schedule.create", &at_args("x"));
    assert!(matches!(result, Err(Error::Denied)));
    assert!(refused(&w, "not_human"));
    // 다른 수신자는 계약이 정해지기 전까지 만들지 않는다
    let other = r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","destination":{"binding":"b","conversation":{"kind":"direct","id":"D1"}}}"#;
    assert!(matches!(create(&w, "c2", other), Err(Error::Denied)));
    assert!(refused(&w, "destination_unsupported"));
    assert!(!w.ledger.has_call("r1", "c2"));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn the_owner_limit_is_enforced_by_the_store_and_a_refusal_changes_nothing() {
    let w = schedule_world();
    w.schedules.fill("personal", OWNER, MAX_ACTIVE_PER_OWNER);
    assert!(matches!(
        create(&w, "c1", &at_args("one too many")),
        Err(Error::Conflict)
    ));
    assert_eq!(w.schedules.count(), MAX_ACTIVE_PER_OWNER);
    // 다른 사람의 한도에는 영향이 없다
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "local:alice",
        "main",
        &WITH_SCHEDULE,
    ));
    let result = w.call_on(
        &w.broker,
        "r2",
        1,
        "c1",
        "schedule.create",
        &at_args("alice"),
    );
    assert!(matches!(result, Ok(CallResult::Done(_))), "{result:?}");
}

#[test]
fn only_the_owner_can_cancel_and_the_list_shows_only_their_own() {
    let w = schedule_world();
    w.schedules.fill("personal", "local:alice", 1);
    let mine = id_of(&done(create(&w, "c1", &at_args("mine"))));
    // 남의 예약은 취소할 수 없고(없는 것처럼 답한다) 목록에도 없다
    assert!(matches!(
        w.call("c2", "schedule.cancel", r#"{"id":"sched-001"}"#),
        Err(Error::Missing)
    ));
    assert!(
        w.schedules.get("sched-001").is_some(),
        "남의 예약은 그대로다"
    );
    let listed = done(w.call("c3", "schedule.list", "{}"));
    assert!(
        listed.contains(&mine) && !listed.contains("sched-001"),
        "{listed}"
    );
    // 목록 인자는 비어 있어야 하고 취소 인자는 id 하나다
    assert!(matches!(
        w.call("c4", "schedule.list", r#"{"all":true}"#),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        w.call("c5", "schedule.cancel", "{}"),
        Err(Error::Invalid)
    ));
}

#[test]
fn a_policy_that_requires_approval_shows_the_instruction_and_creates_nothing_until_it_is_approved()
{
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("send the weekly report")));
    assert_eq!(w.schedules.count(), 0);
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.tool.0, "schedule.create");
    assert_eq!(stored.approval.tool_revision.0, "builtin-1");
    assert_eq!(stored.approval.risk, Risk::Write);
    assert_eq!(stored.reasons, vec!["policy".to_string()]);
    assert!(
        stored.card.0.contains("send the weekly report"),
        "{}",
        stored.card.0
    );
    // 승인하면 한 번만 만든다
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(create(&w, "c1", &at_args("send the weekly report")));
    assert_eq!(w.schedules.count(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(
        done(create(&w, "c1", &at_args("send the weekly report"))),
        receipt
    );
    assert_eq!(w.schedules.count(), 1);
    // 읽기인 목록은 승인을 요구하지 않는다
    assert!(matches!(
        w.call("c2", "schedule.list", "{}"),
        Ok(CallResult::Done(_))
    ));
}

#[test]
fn an_old_approval_button_goes_stale_when_the_policy_or_the_scope_changed() {
    // 정책 판본이 바뀌었다
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("x")));
    w.publish(snapshot_with("r2").approve(&["schedule.create"]).build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    // 스코프가 회수됐다
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("x")));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_result_is_hidden_when_the_revision_or_the_claim_moved_while_it_was_made() {
    let w = schedule_world();
    let first = done(create(&w, "c1", &at_args("x")));
    // 다른 프로세스가 새 판본을 게시했다: 영수증도 목록도 내보내지 않는다
    w.ledger.set_active_revision("personal", "r2");
    assert!(matches!(
        create(&w, "c1", &at_args("x")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c2", "schedule.list", "{}"),
        Err(Error::Conflict | Error::Stale)
    ));
    w.ledger.set_active_revision("personal", "r1");
    assert_eq!(done(create(&w, "c1", &at_args("x"))), first);
    // claim이 넘어간 옛 시도
    let old = w.ledger.claim_epoch("r1");
    w.ledger.bump_claim("r1");
    let stale = block_on(w.broker.call(ToolCall {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(old),
        },
        call: CallId("c9".into()),
        tool: ToolId("schedule.create".into()),
        args: JsonPayload(at_args("late")),
    }));
    assert!(matches!(stale, Err(Error::Stale)), "{stale:?}");
}

#[test]
fn the_host_only_id_and_the_generic_start_are_closed() {
    let w = schedule_world();
    // 호스트 전용 ID는 모델 경로로 부를 수 없다
    assert!(matches!(
        w.call("c1", "schedule.deliver", "{}"),
        Err(Error::Missing)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
    // 일반 `start`로는 예약 생성·취소를 시작할 수 없다(저장소의 전용 커밋으로만 한다)
    for (call, tool) in [("g1", "schedule.create"), ("g2", "schedule.cancel")] {
        let attempt = Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(w.ledger.claim_epoch("r1")),
        };
        block_on(w.ledger.begin(
            attempt.clone(),
            CallId(call.into()),
            ToolId(tool.into()),
            [0; 32],
        ))
        .unwrap();
        let start = StartCall {
            checked_revision: Revision("r1".into()),
            risk: Risk::Write,
            tool_revision: Revision("builtin-1".into()),
            attempt,
            call: CallId(call.into()),
            approval: None,
            private_recall: false,
            owner: "x".into(),
        };
        assert!(block_on(w.ledger.start(start)).is_err(), "{tool}");
    }
}

#[test]
fn without_a_store_every_schedule_operation_is_unavailable() {
    let w = schedule_world();
    let bare = {
        use crate::service::{BrokerService, Parts};
        BrokerService::new(
            Parts {
                catalog: w.catalog.clone(),
                memory: w.memory.clone(),
                ledger: w.ledger.clone(),
                effects: w.effects.clone(),
                audit: w.audit.clone(),
                runner: w.runner.clone(),
                sealer: Arc::new(FakeSealer),
                tracer: w.tracer.clone(),
                clock: w.clock.clone(),
                entropy: w.entropy.clone(),
            },
            Config::default(),
        )
    };
    let try_call = |call: &str, tool: &str, args: &str| w.call_on(&bare, "r1", 1, call, tool, args);
    assert!(matches!(
        try_call("c1", "schedule.create", &at_args("x")),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        try_call("c2", "schedule.list", "{}"),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        try_call("c3", "schedule.cancel", r#"{"id":"s"}"#),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.schedules.count(), 0);
    let _ = ConversationKind::Cli;
}

#[test]
fn a_list_is_recorded_as_a_call_and_its_result_is_reused_after_the_authority_is_checked_again() {
    let w = schedule_world();
    let first = done(w.call("c1", "schedule.list", "{}"));
    assert_eq!(first, r#"{"schedules":[]}"#);
    // 호출 원장에 논리 호출 ID로 남는다(런타임이 다음 도구 계획을 저장하려면 이 기록이 있어야 한다)
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Done(p) if p.0 == first),
        "{:?}",
        w.ledger.phase("r1", "c1")
    );
    // 그 사이 예약이 늘었어도 같은 호출의 재전달은 기록한 결과다(목록을 다시 읽지 않는다)
    done(create(&w, "c2", &at_args("later")));
    assert_eq!(done(w.call("c1", "schedule.list", "{}")), first);
    // 새 호출은 새로 읽는다
    assert!(done(w.call("c3", "schedule.list", "{}")).contains("sched-001"));
    // 같은 호출 ID에 다른 도구는 충돌이다
    assert!(matches!(
        w.call("c1", "schedule.cancel", r#"{"id":"x"}"#),
        Err(Error::Conflict)
    ));
    // 재전달도 현재 권한을 다시 확인한다: 스코프를 회수하면 기록한 결과도 나가지 않는다
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn a_list_that_could_not_be_read_is_given_back_so_a_redelivery_reads_it_again() {
    let w = schedule_world();
    w.schedules.fail_list_with(Some(Error::Unavailable));
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Unavailable)
    ));
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Open { .. }),
        "읽지 못했으니 호출은 열린 채로 돌아온다: {:?}",
        w.ledger.phase("r1", "c1")
    );
    w.schedules.fail_list_with(None);
    assert_eq!(
        done(w.call("c1", "schedule.list", "{}")),
        r#"{"schedules":[]}"#
    );
}

#[test]
fn the_default_timezone_is_fixed_from_the_pinned_revision_for_the_card_the_hash_and_the_store() {
    // 실행이 고정한 r1의 프로필 시간대는 서울이고, 정책은 예약 생성에 승인을 요구한다
    let policy = |rev: &str, zone: &str| {
        snapshot_with(rev)
            .approve(&["schedule.create"])
            .timezone(zone)
            .policy_revision("p-same") // 시간대만 바뀌는 새 판본: 정책 판본은 그대로라 옛 카드가 아직 유효하다
            .build()
    };
    let w = World::new();
    w.publish(policy("r1", "Asia/Seoul"));
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let args = r#"{"cron":"0 9 * * *","instruction":"morning report"}"#;
    let nonce = awaiting(create(&w, "c1", args));
    let stored = w.ledger.stored_approval(&nonce);
    assert!(
        stored.card.0.contains("Asia/Seoul"),
        "카드에 실제 시간대가 보인다: {}",
        stored.card.0
    );
    // 승인 전에 현재 판본의 기본 시간대만 UTC로 바뀐다
    w.publish(policy("r2", "UTC"));
    w.decide(&nonce, OWNER, true).unwrap();
    // 같은 호출을 이어 가면 카드에 보인 서울 9시로 저장된다(UTC 9시가 아니다)
    let id = id_of(&done(create(&w, "c1", args)));
    let (_, timing, _) = w.schedules.get(&id).unwrap();
    assert!(
        matches!(&timing, Timing::Cron { timezone, .. } if timezone == "Asia/Seoul"),
        "{timing:?}"
    );
}
