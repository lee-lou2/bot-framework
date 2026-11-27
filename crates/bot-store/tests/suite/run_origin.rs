use crate::support::{schedule::*, *};
use bot_core::{
    command::{Delegate, JsonPayload},
    id::{AgentId, Revision, ScheduleId},
    port::{DelegationStore, RunStore},
    schedule::{AdvanceSchedule, NextOccurrence, Wake},
    state::{ConversationKind, RunOrigin},
};

#[tokio::test]
async fn only_the_authenticated_chat_entry_can_set_message() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (principal, kind, origin) in [
        ("local:owner", ConversationKind::Cli, RunOrigin::Message),
        (
            "slack:home/U1",
            ConversationKind::Direct,
            RunOrigin::Message,
        ),
        (
            "slack:home/W1",
            ConversationKind::Channel,
            RunOrigin::Message,
        ),
        ("http:inbox/owner", ConversationKind::Http, RunOrigin::Http),
        ("service:task", ConversationKind::Cli, RunOrigin::Unknown),
        ("local:owner", ConversationKind::Direct, RunOrigin::Unknown),
    ] {
        let mut cmd = accept_as(
            "personal",
            origin.as_str(),
            principal,
            kind.clone(),
            "thread",
            "approve",
        );
        cmd.event.request.0 = format!("{}-{:?}", principal, kind);
        cmd.event.body =
            JsonPayload(r#"{"_meta":{"bot/trigger":"message"},"text":"approve"}"#.into());
        let run = db.store.accept(cmd.clone()).await.unwrap();
        assert_eq!(db.store.accept(cmd).await.unwrap(), run);
        let ctx = db.store.claim().await.unwrap().unwrap();
        assert_eq!(ctx.attempt.run, run);
        assert_eq!(ctx.origin, origin);
        db.store.finish(ctx.attempt, done("ok")).await.unwrap();
    }
}

async fn fire(db: &TestDb, id: &ScheduleId) -> bot_core::id::RunId {
    make_due(db, id, 5_000).await;
    let when = column::<i64>(db, id, "at_ms").await;
    let cursor = column::<i64>(db, id, "cursor_ms").await;
    users(db)
        .advance(AdvanceSchedule {
            profile: profile_id(),
            id: id.clone(),
            expected_cursor: at(cursor),
            checked_revision: Some(Revision("r1".into())),
            next: NextOccurrence::Fire {
                at: at(when),
                next: None,
                scopes: run_scopes(),
            },
        })
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_human_conversation_can_receive_both_kinds_of_automatic_run_and_the_child_inherits_its_root()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let owner = leased(&db, "human").await;
    assert_eq!(owner.origin, RunOrigin::Message);
    let schedule = make(&db, &owner, "create", spec_in(&db, 90000).await)
        .await
        .unwrap();
    begin_call(&db, &owner.attempt, "follow").await;
    calls(&db)
        .start(start_call(&owner.attempt, "follow", None))
        .await
        .unwrap();
    let wake = users(&db)
        .finish_with_wake(
            owner.attempt.clone(),
            call("follow"),
            Revision("r1".into()),
            JsonPayload("{}".into()),
            Wake {
                at: at(db.now_ms().await + 90000),
                instruction: "check".into(),
                quiet: false,
            },
        )
        .await
        .unwrap();
    db.store
        .finish(owner.attempt.clone(), done("saved"))
        .await
        .unwrap();
    for (id, origin) in [(schedule, RunOrigin::Schedule), (wake, RunOrigin::Wake)] {
        let fired = fire(&db, &id).await;
        let parent = db.store.claim().await.unwrap().unwrap();
        assert_eq!(parent.attempt.run, fired);
        assert_eq!(parent.conversation.kind, owner.conversation.kind);
        assert_eq!(parent.principal, owner.principal);
        assert_eq!(parent.origin, origin);
        let delegated = db
            .store
            .delegate(Delegate {
                parent: parent.attempt,
                checked_revision: Revision("r1".into()),
                step: "child".into(),
                child: AgentId("main/research".into()),
                instruction: "check".into(),
                scopes: run_scopes(),
            })
            .await
            .unwrap();
        let child = db.store.claim().await.unwrap().unwrap();
        assert_eq!(child.attempt.run, delegated);
        assert_eq!(child.origin, origin);
        db.store.finish(child.attempt, done("done")).await.unwrap();
        let resumed = db.store.claim().await.unwrap().unwrap();
        assert_eq!(resumed.attempt.run, fired);
        assert_eq!(resumed.origin, origin);
        db.store
            .finish(resumed.attempt, done("done"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn upgrading_existing_runs_and_schedules_never_labels_them_as_human_input() {
    let Some(mut db) = TestDb::new().await else {
        return;
    };
    let owner = leased(&db, "pre-upgrade").await;
    let schedule = make(&db, &owner, "old-schedule", spec_in(&db, 90000).await)
        .await
        .unwrap();
    for sql in [
        "alter table bot_runs drop column run_origin",
        "alter table bot_user_schedules drop column fire_origin",
        "delete from _sqlx_migrations where version = 13",
    ] {
        db.sql(sql).await;
    }
    db.store.migrate().await.unwrap();
    db.reconnect().await;
    let old = calls(&db).context(owner.attempt.clone()).await.unwrap();
    assert_eq!(old.origin, RunOrigin::Unknown);
    db.store.finish(owner.attempt, done("ok")).await.unwrap();
    let fired = fire(&db, &schedule).await;
    let old_schedule = db.store.claim().await.unwrap().unwrap();
    assert_eq!(old_schedule.attempt.run, fired);
    assert_eq!(old_schedule.origin, RunOrigin::Unknown);
}
