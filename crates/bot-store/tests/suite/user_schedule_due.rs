//! User-created schedule firing: due scanning, origin restoration, occurrence deduplication, cancel serialization, and quiet completion.

use crate::support::{schedule::*, *};
use bot_core::{command::*, id::*, ledger::StartCall, port::*, schedule::*, state::*};
use std::collections::BTreeSet;

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
    db.sql("update bot_runs set private_memory_read = true, reply_transport = 'fixture-push'")
        .await;
    let spec = spec_in(&db, 3_600_000).await;
    let expected_instruction = spec.instruction.clone();
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
        .expect("accepted run");
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

    let got = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, run);
    assert_eq!(got.principal.0, OWNER);
    assert_eq!(
        (got.conversation.kind, got.conversation.id.as_str()),
        (ConversationKind::Direct, "c1")
    );
    assert_eq!(
        db.scalar::<String>("select reply_transport from bot_runs where trigger is not null")
            .await,
        "fixture-push"
    );
    assert_eq!(
        got.scopes,
        all(&["run"]),
        "fired run scopes must be a subset of the ceiling"
    );
    assert_eq!(
        got.trigger.as_ref().map(|t| t.id.clone()),
        Some(format!("user:{}", id.0))
    );
    assert!(
        matches!(&got.work.target, Target::Agent { instruction, .. } if instruction == &expected_instruction)
    );
    assert!(
        db.scalar::<bool>(&format!(
            "select private_memory_read from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        "fired run must inherit origin private_memory_read taint"
    );
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
        "WaitUntil must not advance cursor past an unfired occurrence"
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
        (true, " <quiet>\n", 0),
        (true, "no changes", 1),
        (true, "check needed <quiet>", 1),
        (false, "<quiet>", 1),
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

/// Inserts a due schedule directly when the origin run has already finished (calls cannot open on finished runs).
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
                 '{{run,calendar.read}}', {}, 'check', {quiet}, {}, {})",
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
        "text": "<quiet>",
        "attachments": [{"name": "a.txt", "type": "text/plain", "data": "aGk="}]
    })
    .to_string();
    runs(&db)
        .finish(
            got.attempt.clone(),
            Finish {
                result: JsonPayload(serde_json::json!({"text": "<quiet>"}).to_string()),
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
