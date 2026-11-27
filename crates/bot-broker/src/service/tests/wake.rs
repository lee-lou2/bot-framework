//! 도구의 후속 확인 요청(`_meta["bot/wake"]`): 선언한 신뢰 도구만, 결과와 예약을 한 커밋으로, 확정하지 못하면 보류.

use super::*;
use crate::ports::Ran;
use bot_core::ledger::{CallPhase, Outcome};

const SCOPES: [&str; 9] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    "schedule.create",
    "upload.write",
];
const OWNER: &str = "local:owner";
const WAKE: &str =
    r#""bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"verify the upload","quiet":true}"#;

fn result_with(meta: &str) -> Ran {
    Ran::Done(JsonPayload(format!(
        r#"{{"_meta":{{{meta}}},"content":[{{"text":"uploaded","type":"text"}}],"isError":false}}"#
    )))
}

/// `report.upload`: 효과 단계를 쓰는 도구(`effects`)이고 `schedule.create`를 선언했다.
fn waker() -> bot_core::profile::Tool {
    let mut t = tool(Risk::Write, &["upload.write", "schedule.create"]);
    t.effects = true;
    t
}

fn snapshot_with(revision: &str, report: bot_core::profile::Tool) -> SnapshotBuilder {
    base(revision)
        .role("owner", &SCOPES)
        .role(
            "ops",
            &["calendar.delete", "upload.write", "schedule.create"],
        )
        .policy_scopes(&SCOPES)
        .tool("main", "report.upload", report)
}

fn wake_world() -> World {
    let w = World::new();
    w.publish(snapshot_with("r1", waker()).build());
    w.ledger
        .add_run(run_context("personal", "r1", 1, OWNER, "main", &SCOPES));
    w
}

fn upload(w: &World, call: &str) -> Result<CallResult, Error> {
    w.call(call, "report.upload", r#"{"file":"q3"}"#)
}

fn audit_has(w: &World, action: &str, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&(action.to_owned(), word.to_owned()))
}

#[test]
fn a_declared_tool_gets_its_followup_with_the_result_in_one_commit_and_the_model_never_sees_the_meta()
 {
    let w = wake_world();
    w.runner.script(result_with(WAKE));
    let payload = done(upload(&w, "c1"));
    assert_eq!(
        payload, r#"{"content":[{"text":"uploaded","type":"text"}],"isError":false}"#,
        "모델이 보는 결과에 _meta가 없다"
    );
    // 예약은 원래 대화에 한 번(at), quiet까지 요청 그대로다
    let wakes = w.schedules.wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(wakes[0].0, "verify the upload");
    assert_eq!(
        wakes[0].1,
        bot_core::schedule::utc::parse_utc("2030-01-01T12:00:00Z").unwrap()
    );
    assert!(wakes[0].2);
    // 결과는 한 커밋으로 원장에 남았다(떼어 낸 모양 그대로)
    assert!(matches!(&w.ledger.phase("r1", "c1"), CallPhase::Done(p) if p.0 == payload));
    assert_eq!(w.runner.jobs().len(), 1);
    // 같은 논리 호출을 재전달해도 예약은 하나뿐이고 도구도 다시 실행하지 않는다
    assert_eq!(done(upload(&w, "c1")), payload);
    assert_eq!(w.schedules.wakes().len(), 1);
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn other_meta_entries_stay_and_a_result_without_a_request_is_unchanged() {
    let w = wake_world();
    w.runner
        .script(result_with(&format!(r#"{WAKE},"note":"x""#)));
    let payload = done(upload(&w, "c1"));
    assert!(
        payload.contains(r#""_meta":{"note":"x"}"#) && !payload.contains("bot/wake"),
        "{payload}"
    );
    // 요청이 없는 결과는 바이트 그대로다
    w.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[],"isError":false}"#.into(),
    )));
    assert_eq!(done(upload(&w, "c2")), r#"{"content":[],"isError":false}"#);
    w.runner.script(result_with(r#""other":1"#));
    assert!(done(upload(&w, "c3")).contains(r#""_meta":{"other":1}"#));
    assert_eq!(w.schedules.wakes().len(), 1);
}

#[test]
fn a_tool_that_did_not_declare_effects_and_the_scope_cannot_ask_for_a_followup() {
    // 효과 단계를 안 쓰는 도구 / `schedule.create`를 선언하지 않은 도구
    for report in [
        {
            let mut t = waker();
            t.effects = false;
            t
        },
        {
            let mut t = tool(Risk::Write, &["upload.write"]);
            t.effects = true;
            t
        },
    ] {
        let w = World::new();
        w.publish(snapshot_with("r1", report).build());
        w.ledger
            .add_run(run_context("personal", "r1", 1, OWNER, "main", &SCOPES));
        w.runner.script(result_with(WAKE));
        let payload = done(upload(&w, "c1"));
        assert!(!payload.contains("bot/wake"), "{payload}");
        assert!(w.schedules.wakes().is_empty());
        assert!(audit_has(&w, "call.wake", "not_declared"));
    }
}

#[test]
fn an_error_result_is_not_a_success_so_its_request_is_dropped() {
    let w = wake_world();
    w.runner.script(Ran::Done(JsonPayload(format!(
        r#"{{"_meta":{{{WAKE}}},"content":[{{"text":"failed","type":"text"}}],"isError":true}}"#
    ))));
    let payload = done(upload(&w, "c1"));
    assert!(payload.contains(r#""isError":true"#) && !payload.contains("bot/wake"));
    assert!(w.schedules.wakes().is_empty());
    assert!(audit_has(&w, "call.wake", "ignored_error"));
}

#[test]
fn a_request_a_declared_tool_makes_but_cannot_be_read_holds_the_call() {
    for bad in [
        r#""bot/wake":{"at":"1970-01-02T00:00:00Z","instruction":"x"}"#, // 장치의 시계보다 앞선 시각
        r#""bot/wake":{"at":"2030-01-01T12:00:00+09:00","instruction":"x"}"#,
        r#""bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"   "}"#,
        r#""bot/wake":{"at":"2030-01-01T12:00:00Z"}"#,
        r#""bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"x","cron":"* * * * *"}"#,
        r#""bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"x","destination":{}}"#,
        r#""bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"x","quiet":"yes"}"#,
        r#""bot/wake":"soon""#,
    ] {
        let w = wake_world();
        w.runner.script(result_with(bad));
        let result = upload(&w, "c1");
        assert!(is_held(&result), "{bad}: {result:?}");
        assert!(w.schedules.wakes().is_empty(), "{bad}");
        assert!(
            matches!(&w.ledger.phase("r1", "c1"), CallPhase::Held { reason, .. } if reason == "wake_invalid"),
            "{bad}"
        );
        assert_eq!(w.runner.jobs().len(), 1, "도구는 한 번 실행됐다");
    }
}

#[test]
fn a_followup_needs_the_requester_to_still_hold_the_scope_and_be_a_person() {
    // 실행하는 동안 판본이 바뀌어 요청자가 `schedule.create`를 잃었다
    let w = wake_world();
    w.runner.script(result_with(WAKE));
    let (catalog, ledger) = (w.catalog.clone(), w.ledger.clone());
    w.runner.on_run(move || {
        let lowered = snapshot_with("r2", waker())
            .role(
                "owner",
                &["run", "calendar.read", "upload.write", "calendar.write"],
            )
            .policy_scopes(&SCOPES)
            .build();
        catalog.publish(lowered);
        ledger.set_active_revision("personal", "r2");
    });
    let result = upload(&w, "c1");
    assert!(is_held(&result), "{result:?}");
    assert!(w.schedules.wakes().is_empty());
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Held { reason, .. } if reason == "wake_denied")
    );
    // 서비스 주체(정적 예약의 실행)는 예약 주체가 될 수 없다
    let w = wake_world();
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &SCOPES,
    ));
    w.runner.script(result_with(WAKE));
    let result = w.call_on(
        &w.broker,
        "r2",
        1,
        "c1",
        "report.upload",
        r#"{"file":"q3"}"#,
    );
    assert!(is_held(&result), "{result:?}");
    assert!(w.schedules.wakes().is_empty());
}

#[test]
fn when_the_store_cannot_confirm_the_call_is_held_unless_the_commit_is_found() {
    // 저장소 장애 / 한도: 확정하지 못했으니 보류한다(예약은 없다)
    for failure in [Error::Unavailable, Error::Conflict, Error::Denied] {
        let w = wake_world();
        w.schedules.fail_wake_with(Some(failure));
        w.runner.script(result_with(WAKE));
        let result = upload(&w, "c1");
        assert!(is_held(&result), "{result:?}");
        assert!(w.schedules.wakes().is_empty());
        assert!(
            matches!(&w.ledger.phase("r1", "c1"), CallPhase::Held { reason, .. } if reason == "wake_unconfirmed")
        );
        assert_eq!(w.runner.jobs().len(), 1);
    }
    // 커밋했는데 응답을 잃었다: 기록을 먼저 보고 완료돼 있으면 그 결과를 돌려준다(보류하지 않는다)
    let w = wake_world();
    w.schedules.lose_wake_response(true);
    w.runner.script(result_with(WAKE));
    let payload = done(upload(&w, "c1"));
    assert!(!payload.contains("bot/wake"));
    assert_eq!(w.schedules.wakes().len(), 1);
    assert!(matches!(&w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
    // 한도(32)에 걸리면 보류한다
    let w = wake_world();
    w.schedules.fill("personal", OWNER, 32);
    w.runner.script(result_with(WAKE));
    assert!(is_held(&upload(&w, "c1")));
}

#[test]
fn unsettled_effect_steps_hold_the_call_before_any_followup_is_made() {
    let w = wake_world();
    w.effects
        .set_unresolved(&[("e1", bot_core::state::EffectState::Held, Some("lost"))]);
    w.runner.script(result_with(WAKE));
    let result = upload(&w, "c1");
    assert!(is_held(&result), "{result:?}");
    assert!(w.schedules.wakes().is_empty());
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Held { reason, .. } if reason == "effects_unresolved")
    );
}

#[test]
fn the_runner_payload_shape_round_trips_through_the_extraction() {
    use super::super::wake::extract;
    let now = bot_core::schedule::utc::parse_utc("2029-12-31T00:00:00Z").unwrap();
    let payload = JsonPayload(
        r#"{"_meta":{"bot/wake":{"at":"2030-01-01T12:00:00Z","instruction":"x"}},"content":[],"isError":false}"#.into(),
    );
    let out = extract(&payload, now);
    assert_eq!(out.payload.0, r#"{"content":[],"isError":false}"#);
    assert!(matches!(out.request, Some(Ok(_))) && !out.is_error);
    // 모양이 다른 결과(객체가 아님)는 요청이 없다
    for text in ["[]", "\"x\"", "not json", r#"{"content":[]}"#] {
        let out = extract(&JsonPayload(text.into()), now);
        assert!(out.request.is_none() && out.payload.0 == text, "{text}");
    }
    let _ = Outcome::Blocked;
}

#[test]
fn the_current_declaration_of_the_tool_must_still_allow_a_followup_before_the_commit() {
    // 실행하는 동안 새 판본이 도구의 `schedule.create` 선언(또는 `effects`)을 철회했다. 요청자의 스코프는 그대로다.
    let revoked = [
        {
            let mut t = waker();
            t.scopes.remove("schedule.create");
            t
        },
        {
            let mut t = waker();
            t.effects = false;
            t
        },
    ];
    for tool in revoked {
        let w = wake_world();
        w.runner.script(result_with(WAKE));
        let (catalog, ledger) = (w.catalog.clone(), w.ledger.clone());
        w.runner.on_run(move || {
            catalog.publish(snapshot_with("r2", tool.clone()).build());
            ledger.set_active_revision("personal", "r2");
        });
        let result = upload(&w, "c1");
        assert!(is_held(&result), "{result:?}");
        assert!(w.schedules.wakes().is_empty());
        assert!(
            matches!(&w.ledger.phase("r1", "c1"), CallPhase::Held { reason, .. } if reason == "wake_denied")
        );
    }
}
