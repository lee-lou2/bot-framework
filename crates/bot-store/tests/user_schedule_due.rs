//! 사용자 예약의 발화(PG): 확인할 예약 읽기, 발생 접수(원래 대화·신원 복원), 커서·유일키 수렴, 취소와의 직렬화, 조용한 완료.

mod support;

use bot_core::{command::*, id::*, ledger::StartCall, port::*, schedule::*, state::*};
use std::collections::BTreeSet;
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

fn all(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn fire(at_ms: i64, next: Option<i64>, scopes: &[&str]) -> NextOccurrence {
    NextOccurrence::Fire {
        at: at(at_ms),
        next: next.map(at),
        scopes: all(scopes),
    }
}

fn advance(
    id: &ScheduleId,
    cursor: i64,
    revision: Option<&str>,
    next: NextOccurrence,
) -> AdvanceSchedule {
    AdvanceSchedule {
        profile: profile_id(),
        id: id.clone(),
        expected_cursor: at(cursor),
        checked_revision: revision.map(|r| Revision(r.into())),
        next,
    }
}

/// 소유자의 DM에서 만든 한 번짜리 예약을 이미 지난 시각으로 옮겨 둔다. (예약, 예정 시각, 커서)
async fn due_at(
    db: &TestDb,
    run: &RunContext,
    call_id: &str,
    quiet: bool,
) -> (ScheduleId, i64, i64) {
    let mut spec = spec_in(db, 3_600_000).await;
    spec.quiet = quiet;
    let id = make(db, run, call_id, spec).await.unwrap();
    make_due(db, &id, 5_000).await;
    let at_ms = column::<i64>(db, &id, "at_ms").await;
    let cursor = column::<i64>(db, &id, "cursor_ms").await;
    (id, at_ms, cursor)
}

#[tokio::test]
async fn firing_makes_one_run_in_the_original_conversation_and_closes_a_one_shot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set private_memory_read = true")
        .await; // 원 요청 트리가 private 기억을 읽었다
    let spec = spec_in(&db, 3_600_000).await;
    begin_tool(&db, &origin, "c1", "schedule.create", sealed(&spec)).await;
    let recall = StartCall {
        private_recall: true,
        ..start_call(&origin.attempt, "c1", None)
    };
    let id = users(&db).create(recall, spec).await.unwrap();
    make_due(&db, &id, 5_000).await;
    let at_ms = column::<i64>(&db, &id, "at_ms").await;
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;

    let page = users(&db).due(None, 10).await.unwrap();
    assert_eq!(page.schedules.len(), 1);
    let due = &page.schedules[0];
    assert_eq!((due.id.clone(), ms(due.cursor)), (id.clone(), cursor));
    assert_eq!(due.owner.0, OWNER);
    assert_eq!(due.origin.conversation.id, "c1");
    assert_eq!(due.ceiling, run_scopes());
    assert!(matches!(due.spec.timing, Timing::At(t) if ms(t) == at_ms));

    let run = users(&db)
        .advance(advance(
            &id,
            cursor,
            Some("r1"),
            fire(at_ms, None, &["run"]),
        ))
        .await
        .unwrap()
        .expect("접수한 run");
    // 같은 발생을 다른 스케줄러가 다시 접수해도 같은 run이다
    let again = users(&db)
        .advance(advance(
            &id,
            cursor,
            Some("r1"),
            fire(at_ms, None, &["run"]),
        ))
        .await
        .unwrap();
    assert_eq!(again, Some(run.clone()));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        1
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where trigger is not null")
            .await,
        1
    );

    // 원래 대화로 돌아가 원래 신원으로 돈다
    let got = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, run);
    assert_eq!(got.principal.0, OWNER);
    assert_eq!(
        (got.conversation.kind, got.conversation.id.as_str()),
        (ConversationKind::Direct, "c1")
    );
    assert_eq!(
        got.scopes,
        all(&["run"]),
        "발화 시점의 권한(상한의 부분집합)"
    );
    assert_eq!(
        got.trigger.as_ref().map(|t| t.id.clone()),
        Some(format!("user:{}", id.0))
    );
    assert!(
        matches!(&got.work.target, Target::Agent { instruction, .. } if instruction == "일정을 확인해 줘")
    );
    assert!(
        db.scalar::<bool>(&format!(
            "select private_memory_read from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        "원 요청의 private 표지를 이어 받는다"
    );
    // 한 번짜리는 마지막 발생으로 닫힌다
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    assert_eq!(column::<String>(&db, &id, "close_reason").await, "finished");
    assert!(users(&db).due(None, 10).await.unwrap().schedules.is_empty());
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'schedule.fire'")
            .await,
        1
    );
}

#[tokio::test]
async fn an_advance_from_a_stale_view_a_fast_clock_or_a_wider_scope_is_refused() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let (id, at_ms, cursor) = due_at(&db, &origin, "c1", false).await;
    let now = db.now_ms().await;
    let attempt = |next| advance(&id, cursor, Some("r1"), next);
    let users = users(&db);
    // 아직 오지 않은 시각·커서 뒤로 가는 시각·한 번짜리의 다른 시각·다음 발생·상한을 넘는 권한은 받지 않는다
    for next in [
        fire(now + 3_600_000, None, &["run"]),
        fire(cursor, None, &["run"]),
        fire(at_ms - 1, None, &["run"]),
        fire(at_ms, Some(now + 60_000), &["run"]),
        fire(at_ms, None, &["run", "calendar.delete"]),
    ] {
        assert!(matches!(
            users.advance(attempt(next)).await,
            Err(Error::Invalid)
        ));
    }
    // 낡은 판본·판본 없는 발화
    assert!(matches!(
        users
            .advance(advance(
                &id,
                cursor,
                Some("r0"),
                fire(at_ms, None, &["run"])
            ))
            .await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        users
            .advance(advance(&id, cursor, None, fire(at_ms, None, &["run"])))
            .await,
        Err(Error::Invalid)
    ));
    // 다른 스케줄러가 커서를 먼저 옮긴 뒤 같은 발생이 아닌 입력은 낡았다
    assert!(matches!(
        users
            .advance(advance(
                &id,
                cursor - 1,
                Some("r1"),
                fire(at_ms, None, &["run"])
            ))
            .await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        0
    );
    assert!(matches!(
        users
            .advance(advance(
                &ScheduleId("none".into()),
                cursor,
                Some("r1"),
                NextOccurrence::Close(CloseReason::Finished)
            ))
            .await,
        Err(Error::Missing)
    ));
}

#[tokio::test]
async fn a_cron_schedule_waits_for_its_first_occurrence_then_fires_and_moves_on() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_cron("0 9 * * *", "Asia/Seoul"))
        .await
        .unwrap();
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    // 만든 직후 곧바로 확인 대상이다. 첫 발생이 미래면 커서를 넘기지 않고 그 시각까지 기다린다
    assert_eq!(users(&db).due(None, 10).await.unwrap().schedules.len(), 1);
    let now = db.now_ms().await;
    let first = now + 3_600_000;
    users(&db)
        .advance(advance(
            &id,
            cursor,
            Some("r1"),
            NextOccurrence::WaitUntil(at(first)),
        ))
        .await
        .unwrap();
    assert!(users(&db).due(None, 10).await.unwrap().schedules.is_empty());
    assert_eq!(
        column::<i64>(&db, &id, "cursor_ms").await,
        cursor,
        "발생을 버리지 않는다"
    );
    assert!(matches!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                NextOccurrence::WaitUntil(at(cursor))
            ))
            .await,
        Err(Error::Invalid)
    ));
    // 시간이 흘러 발생 시각이 되면 접수하고 다음 확인 시각으로 간다(반복은 닫히지 않는다)
    db.sql("update bot_user_schedules set cursor_ms = cursor_ms - 7200000, next_ms = bot_now_ms() - 1000")
        .await;
    let cursor = cursor - 7_200_000;
    let hit = now - 1_000;
    let run = users(&db)
        .advance(advance(
            &id,
            cursor,
            Some("r1"),
            fire(hit, Some(now + 86_400_000), &["run"]),
        ))
        .await
        .unwrap();
    assert!(run.is_some());
    assert_eq!(column::<i64>(&db, &id, "cursor_ms").await, hit);
    assert_eq!(column::<String>(&db, &id, "state").await, "active");
    assert!(users(&db).due(None, 10).await.unwrap().schedules.is_empty());
    // 앞선 입력(옛 커서)의 다른 발생은 낡았다
    assert!(matches!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                fire(hit - 500, None, &["run"])
            ))
            .await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn cancelling_and_firing_serialize_and_close_is_idempotent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let (id, at_ms, cursor) = due_at(&db, &origin, "c1", false).await;
    cancel_with(&db, &origin, "k1", &id).await.unwrap();
    // 취소한 뒤에는 발화하지 않는다
    assert!(matches!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                fire(at_ms, None, &["run"])
            ))
            .await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                NextOccurrence::WaitUntil(at(at_ms + 1))
            ))
            .await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        0
    );
    // 닫기는 이미 이뤄진 일이다
    assert_eq!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                NextOccurrence::Close(CloseReason::Finished)
            ))
            .await
            .unwrap(),
        None
    );
    // 이미 접수한 발생은 취소해도 그 run으로 추적된다
    let origin2 = run_in(&db, "r2", OWNER, ConversationKind::Direct).await;
    let (id2, at2, cursor2) = due_at(&db, &origin2, "c2", false).await;
    let run = users(&db)
        .advance(advance(
            &id2,
            cursor2,
            Some("r1"),
            fire(at2, None, &["run"]),
        ))
        .await
        .unwrap();
    cancel_with(&db, &origin2, "k2", &id2).await.unwrap();
    assert_eq!(
        users(&db)
            .advance(advance(
                &id2,
                cursor2,
                Some("r1"),
                fire(at2, None, &["run"])
            ))
            .await
            .unwrap(),
        run
    );
}

#[tokio::test]
async fn closing_without_a_revision_is_only_for_a_deactivated_profile() {
    use bot_core::profile::ActiveRevisions;
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let (id, _, cursor) = due_at(&db, &origin, "c1", false).await;
    assert!(matches!(
        users(&db)
            .advance(advance(
                &id,
                cursor,
                None,
                NextOccurrence::Close(CloseReason::ProfileInactive)
            ))
            .await,
        Err(Error::Stale)
    ));
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            Default::default(),
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
    users(&db)
        .advance(advance(
            &id,
            cursor,
            None,
            NextOccurrence::Close(CloseReason::ProfileInactive),
        ))
        .await
        .unwrap();
    assert_eq!(
        column::<String>(&db, &id, "close_reason").await,
        "profile_inactive"
    );
    assert_eq!(column::<String>(&db, &id, "state").await, "closed");
    assert!(users(&db).due(None, 10).await.unwrap().schedules.is_empty());
}

#[tokio::test]
async fn a_quiet_completion_sends_no_reply_but_anything_else_still_does() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    for (n, (quiet, text, replies)) in [
        (true, " <조용히>\n", 0),
        (true, "변경 없음", 1),
        (true, "확인 필요 <조용히>", 1),
        (false, "<조용히>", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let (id, at_ms, cursor) = due_at_ok(&db, &origin, &format!("c{n}"), quiet).await;
        users(&db)
            .advance(advance(
                &id,
                cursor,
                Some("r1"),
                fire(at_ms, None, &["run"]),
            ))
            .await
            .unwrap();
        let got = runs(&db).claim().await.unwrap().unwrap();
        runs(&db)
            .finish(got.attempt.clone(), done(text))
            .await
            .unwrap();
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select count(*) from bot_outbox where run = '{}'",
                got.attempt.run.0
            ))
            .await,
            replies,
            "quiet={quiet} text={text:?}"
        );
        assert_eq!(state_of_run(&db, &got.attempt.run).await, "succeeded");
    }
}

/// `due_at`과 같지만 원래 run이 이미 끝난 상태에서 호출을 여는 시험용(호출은 끝난 run에서 못 연다): 예약만 직접 넣는다.
async fn due_at_ok(
    db: &TestDb,
    origin: &RunContext,
    call_id: &str,
    quiet: bool,
) -> (ScheduleId, i64, i64) {
    let now = db.now_ms().await;
    let id: String = db.scalar("select gen_random_uuid()::text").await;
    db.sql(&format!(
        "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, at_ms, instruction, quiet, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', '{call_id}', 'desk', 'direct', 'c1',
                 '{{run,calendar.read}}', {}, '확인', {quiet}, {}, {})",
        origin.attempt.run.0,
        now - 5_000,
        now - 65_000,
        now - 5_000
    ))
    .await;
    (ScheduleId(id), now - 5_000, now - 65_000)
}

#[tokio::test]
async fn a_quiet_marker_that_carries_an_attachment_is_still_sent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, finished_ms = bot_now_ms()")
        .await;
    let (id, at_ms, cursor) = due_at_ok(&db, &origin, "c1", true).await;
    users(&db)
        .advance(advance(
            &id,
            cursor,
            Some("r1"),
            fire(at_ms, None, &["run"]),
        ))
        .await
        .unwrap();
    let got = runs(&db).claim().await.unwrap().unwrap();
    let answer = serde_json::json!({
        "text": "<조용히>",
        "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]
    })
    .to_string();
    runs(&db)
        .finish(
            got.attempt.clone(),
            Finish {
                result: JsonPayload(serde_json::json!({"text": "<조용히>"}).to_string()),
                answers: vec![JsonPayload(answer)],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_outbox where run = '{}'",
            got.attempt.run.0
        ))
        .await,
        1
    );
}
