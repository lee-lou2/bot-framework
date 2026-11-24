//! 구판 봉인(`quiet`·`at` 표기를 모델이 쓴 그대로 묶던 지문)으로 끝난 `schedule.create` 호출의 재전달.
//! 완료한 호출은 정확히 구판 지문이고 지금도 인가될 때만 저장된 영수증을 읽는다. 열린·시작한 호출의 구판 지문은 쓰지 않는다.

use super::{
    schedule::{AT, WITH_SCHEDULE, at_args, create, schedule_world, snapshot_with},
    *,
};
use crate::canon::sha256;
use bot_core::ledger::{CallLedger, CallPhase};

/// 구판이 `{"at":…,"instruction":"once"}`에 대해 장부에 남긴 지문(`quiet` 없음, 인자 글 그대로의 정규 JSON).
fn old_hash() -> [u8; 32] {
    sha256(&format!(r#"{{"at":"{AT}","instruction":"once"}}"#))
}

/// 구판이 남긴 호출을 장부에 심는다(같은 `begin`으로 만들고 필요하면 완료한다).
fn plant(w: &World, call: &str, tool: &str, hash: [u8; 32]) {
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        claim: Some(w.ledger.claim_epoch("r1")),
        generation: Generation(1),
    };
    block_on(
        w.ledger
            .begin(attempt, CallId(call.into()), ToolId(tool.into()), hash),
    )
    .expect("begin");
}

#[test]
fn a_call_finished_under_the_old_seal_replays_its_receipt_and_nothing_else() {
    let w = schedule_world();
    plant(&w, "old1", "schedule.create", old_hash());
    w.ledger
        .force_done("r1", "old1", r#"{"schedule":"sched-007"}"#);
    // 같은 인자의 재전달: 저장된 영수증만 읽고 아무것도 만들지 않는다
    assert_eq!(
        done(create(&w, "old1", &at_args("once"))),
        r#"{"schedule":"sched-007"}"#
    );
    assert_eq!(w.schedules.count(), 0);
    assert_eq!(
        w.ledger.call_hash("r1", "old1"),
        old_hash(),
        "장부의 지문은 그대로"
    );
    // 다른 인자·다른 도구는 구판 호출이라도 충돌이다
    assert!(matches!(
        create(&w, "old1", &at_args("something else")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("old1", "schedule.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("old1", "schedule.cancel", r#"{"id":"sched-007"}"#),
        Err(Error::Conflict)
    ));
    // 예정 시각이 한참 지난 뒤의 재전달도 영수증이다(미래 검사는 새 호출에만 한다)
    w.clock.set(2_000_000_000 + 1_800_000_000);
    assert_eq!(
        done(create(&w, "old1", &at_args("once"))),
        r#"{"schedule":"sched-007"}"#
    );
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn the_old_fingerprint_follows_the_old_sealing_for_a_filled_in_timezone_too() {
    let w = schedule_world();
    // 구판: cron에 시간대를 생략하면 고정 판본의 프로필 시간대(UTC)를 채운 인자의 정규 JSON을 묶었다.
    let old = sha256(r#"{"cron":"0 9 * * *","instruction":"standup","timezone":"UTC"}"#);
    plant(&w, "old2", "schedule.create", old);
    w.ledger
        .force_done("r1", "old2", r#"{"schedule":"sched-008"}"#);
    assert_eq!(
        done(create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","instruction":"standup"}"#
        )),
        r#"{"schedule":"sched-008"}"#
    );
    // 시간대를 명시한 같은 뜻의 인자도 구판에서는 같은 지문이었다
    assert_eq!(
        done(create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","timezone":"UTC","instruction":"standup"}"#
        )),
        r#"{"schedule":"sched-008"}"#
    );
    // 다른 시간대는 다른 인자다
    assert!(matches!(
        create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","timezone":"Asia/Seoul","instruction":"standup"}"#
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn an_unfinished_call_with_the_old_fingerprint_is_never_reused() {
    let w = schedule_world();
    // 열려 있는 호출: 새 지문으로 다시 시작하지도, 승인을 새로 열지도 않는다
    plant(&w, "open", "schedule.create", old_hash());
    assert!(matches!(
        create(&w, "open", &at_args("once")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "open"),
        CallPhase::Open { approval: None }
    ));
    // 시작한 호출
    plant(&w, "started", "schedule.create", old_hash());
    w.ledger.force_started("r1", "started", 1, "owner.started");
    assert!(matches!(
        create(&w, "started", &at_args("once")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "started"),
        CallPhase::Started { .. }
    ));
    assert_eq!(w.schedules.count(), 0);
    assert!(
        w.audit
            .verdicts()
            .iter()
            .all(|(action, _)| action.as_str() != "call.deny")
    );
    // 저장소가 지문이 다른 기존 호출을 그대로 돌려주더라도(걸러 주지 못하는 경우) 브로커가 한 번 더 거른다
    w.ledger.set_lenient_begin(true);
    for call in ["open", "started"] {
        let result = create(&w, call, &at_args("once"));
        assert!(matches!(result, Err(Error::Conflict)), "{call}: {result:?}");
    }
    assert_eq!(w.schedules.count(), 0);
    assert!(matches!(
        w.ledger.phase("r1", "open"),
        CallPhase::Open { approval: None }
    ));
}

#[test]
fn a_finished_call_is_replayed_only_while_the_requester_is_still_authorized() {
    let w = schedule_world();
    plant(&w, "old3", "schedule.create", old_hash());
    w.ledger
        .force_done("r1", "old3", r#"{"schedule":"sched-009"}"#);
    // 같은 스코프가 철회된 판본: 영수증도 돌려주지 않는다
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        create(&w, "old3", &at_args("once")),
        Err(Error::Denied)
    ));
    // 다시 권한이 생기면 같은 영수증이다(철회가 호출 기록을 바꾸지 않았다)
    w.publish(snapshot_with("r3").build());
    assert_eq!(
        done(create(&w, "old3", &at_args("once"))),
        r#"{"schedule":"sched-009"}"#
    );
    // 서비스 주체의 같은 호출 ID는 영수증을 읽을 수 없다
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    let denied = w.call_on(
        &w.broker,
        "r2",
        1,
        "old3",
        "schedule.create",
        &at_args("once"),
    );
    assert!(matches!(denied, Err(Error::Denied)));
}
